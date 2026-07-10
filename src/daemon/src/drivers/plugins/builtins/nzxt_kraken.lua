-- SPDX-License-Identifier: GPL-3.0-or-later
-- SPDX-FileCopyrightText: liquidctl contributors <https://github.com/liquidctl/liquidctl>
--
-- NZXT Kraken Z / Elite built-in plugin for HaloDaemon — a full port of the
-- native driver's RGB, pump-fan, sensor, accessory-fan-child, and LCD paths. As
-- a built-in it shadows the native NZXT Kraken driver; disabling it falls back to
-- native.
--
-- Protocol references: docs/protocols/ and liquidctl's nzxt_kraken driver.
-- Verified offsets: status report 0x75; Z/Elite lighting 0x26 0x14 (GRB, ring
-- channel 0x01 / accessory channel 0x02); speed profiles 0x72. LCD control 0x30
-- config / 0x36 transfer (+0x37 ACK) / 0x32 0x38 buckets, image over USB bulk.

local REPORT = 64
local RING_LEDS = 24
local RING_SLOTS = 40 -- wire buffer holds 40 GRB slots (120 bytes)
local PROFILE_LEN = 40 -- duty curve is 40 temperature points

-- Cached per-channel GRB buffers (Lua strings). The Z/Elite panel expects the
-- ring and accessory channels streamed together, so both callbacks refresh their
-- own cache and re-send the pair.
local ring_grb = string.rep("\0", RING_SLOTS * 3)
local ext_grb = nil

local function grb_from_colors(colors, slots)
  local b = halod.buffer(slots * 3)
  for i, c in ipairs(colors) do
    local base = (i - 1) * 3
    if base + 2 < slots * 3 then
      b:set_u8(base, c.g)
      b:set_u8(base + 1, c.r)
      b:set_u8(base + 2, c.b)
    end
  end
  return b:tostring()
end

local function lighting_packet(channel_byte, grb)
  local b = halod.buffer(4 + #grb)
  b:set_u8(0, 0x26)
  b:set_u8(1, 0x14)
  b:set_u8(2, channel_byte)
  b:set_u8(3, channel_byte)
  -- copy grb bytes in
  for i = 1, #grb do
    b:set_u8(3 + i, grb:byte(i))
  end
  return b
end

local function send_channels(dev)
  dev.transport:write(lighting_packet(0x01, ring_grb))
  if ext_grb then
    dev.transport:write(lighting_packet(0x02, ext_grb))
  end
end

-- Fixed-duty speed profile: `header` + 40 copies of the clamped duty.
local function duty_packet(h0, h1, h2, h3, duty, min_duty)
  if duty < min_duty then duty = min_duty end
  if duty > 100 then duty = 100 end
  local b = halod.buffer(4 + PROFILE_LEN)
  b:set_u8(0, h0)
  b:set_u8(1, h1)
  b:set_u8(2, h2)
  b:set_u8(3, h3)
  for i = 0, PROFILE_LEN - 1 do
    b:set_u8(4 + i, duty)
  end
  return b
end

-- ── LCD ──────────────────────────────────────────────────────────────────────
-- Image bytes cross a separate USB bulk-OUT endpoint (transport:write_bulk); the
-- 64-byte HID reports carry only the start/stop/config handshake. Every transfer
-- start/end MUST consume its 0x37 ACK or the panel firmware desyncs into the
-- bootloader. Pixel encoding (Q565 / BGR888 / GIF resize) runs in the host via
-- the `halod` codecs — Lua can't touch 100k pixels/frame.

local LCD_BULK_MAGIC = "\x12\xFA\x01\xE8\xAB\xCD\xEF\x98\x76\x54\x32\x10"
local LCD_TOTAL_MEMORY_KB = 24320

-- pid → native panel resolution (all Kraken LCD panels are square/circular).
local LCD_SIZES = {
  [0x3008] = { 320, 320 }, [0x300C] = { 640, 640 }, [0x300E] = { 240, 240 },
  [0x3012] = { 640, 640 }, [0x3014] = { 240, 240 },
}

local lcd_w, lcd_h = 320, 320
-- Cleared whenever the panel leaves raw streaming (Q565 frame, image, default).
local raw_stream_entered = false

-- 20-byte bulk header: magic(12) + asset_mode + 3 zero + LE u32 length.
local function bulk_header(payload_len, asset_mode)
  local b = halod.buffer(20)
  for i = 1, 12 do b:set_u8(i - 1, LCD_BULK_MAGIC:byte(i)) end
  b:set_u8(12, asset_mode)
  b:set_u32_le(16, payload_len)
  return b
end

-- Read up to 8 reports, consuming the matching 0x37 <sub> ACK. Not consuming it
-- desyncs the firmware, so this drains stray reports while searching.
local function await_xfer_ack(dev, sub)
  for _ = 1, 8 do
    local ok, s = pcall(function() return dev.transport:read(REPORT) end)
    if not ok then return false end
    local r = halod.buffer(s)
    if #r >= 2 and r:get_u8(0) == 0x37 and r:get_u8(1) == sub then return true end
  end
  return false
end

local function drain_hid(dev)
  for _ = 1, 64 do
    if #dev.transport:read_nonblocking(REPORT) == 0 then break end
  end
end

-- Write a command and read one reply (any prefix), tolerating no reply.
local function write_then_read(dev, pkt)
  dev.transport:write(pkt)
  pcall(function() dev.transport:read(REPORT) end)
end

local function write_screen_config(dev, brightness, degrees)
  local rot = math.floor((degrees % 360) / 90) % 4
  dev.transport:write(string.char(0x30, 0x02, 0x01, brightness, 0x00, 0x00, 0x01, rot))
end

-- (brightness, degrees) read back from the panel; defaults if it doesn't answer.
local function read_lcd_state(dev)
  dev.transport:write(string.char(0x30, 0x01))
  for _ = 1, 8 do
    local ok, s = pcall(function() return dev.transport:read(REPORT) end)
    if not ok then break end
    local r = halod.buffer(s)
    if #r >= 27 and r:get_u8(0) == 0x31 and r:get_u8(1) == 0x01 then
      local rot_idx = r:get_u8(0x1A)
      if rot_idx > 3 then rot_idx = 3 end
      return r:get_u8(0x18), rot_idx * 90
    end
  end
  return 80, 0
end

-- Asset mode 0x08: Q565-compressed stream.
local function stream_q565(dev, payload)
  drain_hid(dev)
  dev.transport:write(string.char(0x36, 0x01, 0x00, 0x01, 0x08))
  await_xfer_ack(dev, 0x01)
  dev.transport:write_bulk(bulk_header(#payload, 0x08))
  dev.transport:write_bulk(payload)
  dev.transport:write(string.char(0x36, 0x02))
  await_xfer_ack(dev, 0x02)
end

-- Asset mode 0x09: raw BGR888. Per-frame LUTs keep the firmware colour map synced.
local function stream_lut1()
  return string.char(0x72, 0x01, 0x01, 0x00) .. string.rep(string.char(0x3F), 41)
end
local function stream_lut2()
  return string.char(0x72, 0x02, 0x01, 0x01) .. string.rep(string.char(0x1F), 41)
end

-- One-time handshake into live raw-streaming mode; clears all buckets.
local function enter_streaming_mode(dev, brightness)
  local pct = math.min(brightness, 100)
  drain_hid(dev)
  write_then_read(dev, string.char(0x10, 0x02))
  write_then_read(dev, string.char(0x70, 0x02, 0x01, 0xB8, 0x0B))
  write_then_read(dev, string.char(0x74, 0x01))
  write_then_read(dev, string.char(0x36, 0x04))
  write_then_read(dev, string.char(0x30, 0x01))
  write_then_read(dev, string.char(0x36, 0x03))
  write_then_read(dev, string.char(0x30, 0x02, 0x00, 0x00, 0x00, 0x00, 0x1E))
  write_then_read(dev, string.char(0x38, 0x01, 0x02))
  for bi = 0, 15 do write_then_read(dev, string.char(0x32, 0x02, bi)) end
  write_then_read(dev, string.char(0x30, 0x02, 0x01, pct, 0x00, 0x00, 0x00, 0x1E))
  drain_hid(dev)
end

local function stream_raw(dev, bgr888, brightness)
  if not raw_stream_entered then
    enter_streaming_mode(dev, brightness)
    raw_stream_entered = true
  end
  drain_hid(dev)
  dev.transport:write(stream_lut1())
  dev.transport:write(stream_lut2())
  dev.transport:write(string.char(0x36, 0x01, 0x00, 0x01, 0x09))
  await_xfer_ack(dev, 0x01)
  dev.transport:write_bulk(bulk_header(#bgr888, 0x09))
  dev.transport:write_bulk(bgr888)
  dev.transport:write(string.char(0x36, 0x02))
  await_xfer_ack(dev, 0x02)
end

-- ── 16-bucket allocator (GIF / persistent image uploads) ─────────────────────

-- Write pkt, return the first reply matching (pkt[0]+1, pkt[1]).
local function lcd_command(dev, pkt)
  dev.transport:write(pkt)
  local want0 = (pkt:byte(1) + 1) % 256
  local want1 = pkt:byte(2)
  for _ = 1, 24 do
    local ok, s = pcall(function() return dev.transport:read(REPORT) end)
    if not ok then return nil end
    local r = halod.buffer(s)
    if #r >= 2 and r:get_u8(0) == want0 and r:get_u8(1) == want1 then return r end
  end
  return nil
end

local function query_buckets(dev)
  local buckets = {}
  for i = 0, 15 do
    local msg = lcd_command(dev, string.char(0x30, 0x04, i))
    if msg then buckets[i] = msg end
  end
  return buckets
end

-- Lowest bucket whose info (bytes 15+) is all-zero.
local function find_next_unoccupied(buckets)
  for i = 0, 15 do
    local b = buckets[i]
    if b then
      local occupied = false
      for j = 15, #b - 1 do
        if b:get_u8(j) ~= 0 then occupied = true break end
      end
      if not occupied then return i end
    end
  end
  return nil
end

-- LE start offset (1024-byte units) for `data_units` in `bucket_index`.
local function bucket_memory_offset(buckets, bucket_index, data_units)
  local cur = buckets[bucket_index]
  if not cur then return 0 end
  local cur_offset = cur:get_u8(17) | (cur:get_u8(18) << 8)
  local cur_size = cur:get_u8(19) | (cur:get_u8(20) << 8)
  if data_units <= cur_size then return cur_offset end

  local min_occupied, max_occupied, overlap = cur_offset, 0, false
  for idx = 0, 15 do
    local b = buckets[idx]
    if b then
      local start = b:get_u8(17) | (b:get_u8(18) << 8)
      local endo = start + (b:get_u8(19) | (b:get_u8(20) << 8))
      if endo > max_occupied then max_occupied = endo end
      if start < min_occupied then min_occupied = start end
      if (start > cur_offset and start < cur_offset + data_units)
        or (start < cur_offset and endo > cur_offset)
        or (start == cur_offset and idx ~= bucket_index) then
        overlap = true
      end
    end
  end
  if not overlap then return cur_offset end
  if max_occupied + data_units < LCD_TOTAL_MEMORY_KB then return max_occupied end
  if data_units < min_occupied then return 0 end
  return nil -- no room
end

local function delete_bucket(dev, index)
  local msg = lcd_command(dev, string.char(0x32, 0x02, index))
  return msg ~= nil and #msg > 14 and msg:get_u8(14) == 0x01
end

-- Walk forward from bucket_index deleting until landing on a free bucket.
local function prepare_bucket(dev, bucket_index, bucket_filled)
  while true do
    if bucket_index >= 16 then error("Kraken LCD: reached max bucket (16)") end
    if not delete_bucket(dev, bucket_index) then
      bucket_index = bucket_index + 1
      bucket_filled = true
    elseif bucket_filled then
      bucket_filled = false
    else
      return bucket_index
    end
  end
end

local function u16_bytes(v)
  return v & 0xFF, (v >> 8) & 0xFF
end

-- Upload `data` (a ByteBuf) into a memory bucket. `bulk_info` is the 8-byte
-- [asset_mode,0,0,0, len_le32] header tail.
local function run_bucket_pipeline(dev, data, bulk_info)
  local total_len = 12 + #bulk_info + #data
  local data_units = (total_len + 1023) // 1024
  if data_units >= LCD_TOTAL_MEMORY_KB then
    error("Kraken LCD: image too large for panel memory")
  end

  write_then_read(dev, string.char(0x36, 0x03))
  local buckets = query_buckets(dev)
  local found = find_next_unoccupied(buckets)
  local bucket_index = prepare_bucket(dev, found or 0, found == nil)

  local mem_start = bucket_memory_offset(buckets, bucket_index, data_units)
  if mem_start == nil then
    -- No contiguous room: wipe everything and restart at bucket 0.
    lcd_command(dev, string.char(0x38, 0x01, 0x02, 0x00)) -- back to liquid
    for i = 0, 15 do delete_bucket(dev, i) end
    bucket_index, mem_start = 0, 0
  end

  local mem_lo, mem_hi = u16_bytes(mem_start)
  local size_lo, size_hi = u16_bytes(data_units)
  if lcd_command(dev, string.char(0x32, 0x01, bucket_index, bucket_index + 1,
      mem_lo, mem_hi, size_lo, size_hi, 0x01)) == nil then
    error("Kraken LCD: bucket setup was rejected")
  end

  dev.transport:write(string.char(0x36, 0x01, bucket_index))
  await_xfer_ack(dev, 0x01)

  -- header = magic(12) + bulk_info(8); then the data.
  local header = halod.buffer(12 + #bulk_info)
  for i = 1, 12 do header:set_u8(i - 1, LCD_BULK_MAGIC:byte(i)) end
  for i = 1, #bulk_info do header:set_u8(11 + i, bulk_info:byte(i)) end
  dev.transport:write_bulk(header)
  dev.transport:write_bulk(data)

  dev.transport:write(string.char(0x36, 0x02))
  await_xfer_ack(dev, 0x02)
  lcd_command(dev, string.char(0x38, 0x01, 0x04, bucket_index)) -- switch to it
end

local function upload_gif(dev, resized)
  local n = #resized
  local bulk_info = string.char(0x01, 0x00, 0x00, 0x00,
    n & 0xFF, (n >> 8) & 0xFF, (n >> 16) & 0xFF, (n >> 24) & 0xFF)
  run_bucket_pipeline(dev, resized, bulk_info)
end

return {
  match = {
    transport = "hid",
    vid = 0x1E71,
    -- Kraken Z53/63/73, Elite 2023/2024, Kraken 2023/Plus 2024.
    pids = { 0x3008, 0x300C, 0x300E, 0x3012, 0x3014 },
  },
  identity = { vendor = "NZXT", model = "Kraken Z", name = "NZXT Kraken Z" },
  transports = { hid = { report_size = REPORT, timeout_ms = 1000 } },

  rgb = {
    zones = {
      { id = "ring", name = "Pump Ring", topology = { type = "ring" },
        leds = (function()
          local l = {}
          for i = 0, RING_LEDS - 1 do
            local a = (i / RING_LEDS) * 2 * math.pi
            l[#l + 1] = { id = i, x = 0.5 + 0.45 * math.sin(a), y = 0.5 - 0.45 * math.cos(a) }
          end
          return l
        end)() },
    },
  },
  fan = { channel = 0 }, -- the pump (Kraken's own fan capability)
  sensor = {},
  lcd = { needs_rgb_restore = true }, -- uploads reset the ring LEDs
  poll = { interval_ms = 500 },

  chain = {
    channels = { { id = "0", name = "Aer/F Fan", max_leds = 40 } },
    accessories = {
      { id = 0x13, name = "F120 RGB", led_count = 8, topology = "ring", fan = true },
      { id = 0x14, name = "F140 RGB", led_count = 8, topology = "ring", fan = true },
      { id = 0x17, name = "F140 RGB Core", led_count = 8, topology = "ring", fan = true },
      { id = 0x18, name = "F140 RGB Core", led_count = 8, topology = "ring", fan = true },
      { id = 0x1B, name = "F240 RGB Core", led_count = 16, topology = "rings", rings = 2, fan = true },
      { id = 0x1C, name = "F240 RGB Core", led_count = 16, topology = "rings", rings = 2, fan = true },
      { id = 0x1D, name = "F360 RGB Core", led_count = 24, topology = "rings", rings = 3, fan = true },
      { id = 0x1E, name = "F360 RGB Core", led_count = 24, topology = "rings", rings = 3, fan = true },
      { id = 0x1F, name = "F420 RGB Core", led_count = 24, topology = "rings", rings = 3, fan = true },
    },
  },

  initialize = function(dev)
    dev.transport:write(string.char(0x70, 0x02, 0x01, 0xB8, 0x01)) -- INIT_SET
    dev.transport:write(string.char(0x70, 0x01))                   -- firmware push
    dev.transport:write(string.char(0x10, 0x01))                   -- enable status stream
    local size = LCD_SIZES[dev.match.pid] or { 320, 320 }
    lcd_w, lcd_h = size[1], size[2]
    local brightness, rotation = read_lcd_state(dev)
    log("NZXT Kraken Z initialized")
    return {
      ok = true,
      lcd = {
        shape = "circle", width = lcd_w, height = lcd_h,
        rotations = { 0, 90, 180, 270 },
        image_types = { "image/png", "image/jpeg", "image/gif" },
        latches = true,
        brightness = brightness, rotation = rotation,
      },
    }
  end,

  -- Pump ring RGB.
  write_frame = function(dev, zone_id, colors)
    ring_grb = grb_from_colors(colors, RING_SLOTS)
    send_channels(dev)
  end,
  apply = function(dev, state)
    if state.mode == "static" then
      local fill = {}
      for i = 1, RING_LEDS do fill[i] = state.color end
      ring_grb = grb_from_colors(fill, RING_SLOTS)
      send_channels(dev)
    end
  end,

  -- Accessory (F-fan) RGB, composited into the accessory channel by the host.
  write_ext_frame = function(dev, channel, colors)
    ext_grb = grb_from_colors(colors, #colors)
    send_channels(dev)
  end,

  -- Pump duty (min 20%).
  set_duty = function(dev, duty)
    dev.transport:write(duty_packet(0x72, 0x01, 0x00, 0x00, duty, 20))
  end,
  get_duty = function(dev) return (dev.status or {}).pump_duty or 0 end,
  get_rpm = function(dev) return (dev.status or {}).pump_rpm end,

  -- Accessory fan (routed from the child via the parent's fan hub).
  set_fan_duty = function(dev, ch, duty)
    dev.transport:write(duty_packet(0x72, 0x02, 0x01, 0x01, duty, 0))
  end,
  fan_duty = function(dev, ch) return (dev.status or {}).fan_duty or 0 end,
  fan_rpm = function(dev, ch) return (dev.status or {}).fan_rpm or 0 end,
  fan_controllable = function(dev, ch) return ((dev.status or {}).fan_rpm or 0) > 0 end,

  -- Status stream (0x75): liquid temp, pump + fan rpm/duty.
  read_status = function(dev)
    local r = halod.buffer(dev.transport:read_nonblocking(REPORT))
    if #r < 26 or r:get_u8(0) ~= 0x75 then
      return dev.status -- keep last good reading
    end
    local frac = r:get_u8(16)
    if frac > 9 then frac = 9 end
    return {
      liquid_temp = r:get_u8(15) + frac / 10.0,
      pump_rpm = r:get_u16_le(17),
      pump_duty = r:get_u8(19),
      fan_rpm = r:get_u16_le(23),
      fan_duty = r:get_u8(25),
    }
  end,

  get_sensors = function(dev)
    local s = dev.status or {}
    return {
      { id = "liquid", name = "Liquid Temperature", value = s.liquid_temp or 0,
        unit = "celsius", sensor_type = "temperature" },
    }
  end,

  -- Accessory detection (0x20 0x03 -> 0x21 0x03); accessory id at byte 15.
  detect_accessories = function(dev)
    dev.transport:write(string.char(0x20, 0x03))
    for _ = 1, 8 do
      local reply = halod.buffer(dev.transport:read(REPORT))
      if #reply >= 16 and reply:get_u8(0) == 0x21 and reply:get_u8(1) == 0x03 then
        local acc = reply:get_u8(15)
        if acc ~= 0 then
          return { { channel = 0, accessory = acc } }
        end
        return {}
      end
    end
    return {}
  end,

  -- ── LCD callbacks (host passes rotation/brightness/mode from the slot) ──

  -- One rendered engine frame (raw RGBA at native resolution). Pre-rotate in
  -- software (firmware rotates only its built-in display), then Q565 or raw BGR.
  lcd_stream_frame = function(dev, rgba, width, height, rotation, raw, brightness)
    local rotated = halod.rgba_rotate_square(rgba, width, rotation)
    if raw then
      stream_raw(dev, halod.rgba_to_bgr888(rotated), brightness)
    else
      raw_stream_entered = false
      stream_q565(dev, halod.rgba_to_q565(rotated, width, height))
    end
  end,

  -- Upload a still image or animated GIF.
  set_image = function(dev, data, rotation)
    if data:tostring():sub(1, 4) == "GIF8" then
      upload_gif(dev, halod.gif_resize(data, lcd_w, lcd_h))
    else
      raw_stream_entered = false
      local rgba = halod.rgba_rotate_square(halod.image_decode(data, lcd_w, lcd_h), lcd_w, rotation)
      stream_q565(dev, halod.rgba_to_q565(rgba, lcd_w, lcd_h))
    end
  end,

  lcd_set_brightness = function(dev, brightness, rotation)
    write_screen_config(dev, brightness, rotation)
  end,
  lcd_set_rotation = function(dev, brightness, degrees)
    write_screen_config(dev, brightness, degrees)
  end,
  lcd_reset = function(dev)
    raw_stream_entered = false
    dev.transport:write(string.char(0x38, 0x01, 0x02, 0x00)) -- back to built-in display
    pcall(function() dev.transport:read(REPORT) end)
  end,
}
