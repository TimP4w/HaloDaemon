-- SPDX-License-Identifier: GPL-3.0-or-later
-- SPDX-FileCopyrightText: liquidctl contributors <https://github.com/liquidctl/liquidctl>
--
-- NZXT Kraken Z / Elite plugin for HaloDaemon — a port of the native driver's
-- RGB, pump-fan, sensor, and accessory-fan-child paths. LCD is intentionally NOT
-- handled here (the native driver still owns it); disabling this plugin returns
-- the device to native so the LCD keeps working.
--
-- Protocol references: docs/protocols/ and liquidctl's nzxt_kraken driver.
-- Verified offsets: status report 0x75; Z/Elite lighting 0x26 0x14 (GRB, ring
-- channel 0x01 / accessory channel 0x02); speed profiles 0x72.

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
    log("NZXT Kraken Z initialized")
    return true
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
}
