-- SPDX-License-Identifier: GPL-3.0-or-later
-- SPDX-FileCopyrightText: liquidctl contributors <https://github.com/liquidctl/liquidctl>
--
-- NZXT Kraken X53/X63/X73 built-in plugin for HaloDaemon — the older X-series
-- wire protocol, distinct from (and simpler than) the Z/Elite family in
-- `nzxt_kraken.lua`: an 8-LED ring plus a single logo LED, no LCD, and no
-- software pump/fan speed control. This is the only driver for these two
-- PIDs (there is no native Rust fallback); disabling it makes the device
-- undetected.
--
-- Protocol references: docs/protocols/ and liquidctl's kraken2 driver.
-- Verified offsets: status report 0x75 (shared with Z/Elite); RGB `0x22 0x10`
-- per-channel data packets (60 bytes/packet, two packets for 8 LEDs) + `0x22
-- 0xA0` commit; logo `0x2A 0x04`.

local REPORT = 64

local function grb_string(colors)
  local parts = {}
  for _, c in ipairs(colors) do
    parts[#parts + 1] = string.char(c.g, c.r, c.b)
  end
  return table.concat(parts)
end

-- Two 64-byte `0x22 0x10|n` data packets (60 GRB bytes each) plus a 16-byte
-- `0x22 0xA0` commit, for channel `ch` (0x02 = ring, 0x01 = ext accessory).
local function write_x3_channel(dev, ch, colors)
  local grb = grb_string(colors)
  for pkt_num = 0, 1 do
    local off = pkt_num * 60
    local b = halod.buffer(64)
    b:set_u8(0, 0x22)
    b:set_u8(1, 0x10 | pkt_num)
    b:set_u8(2, ch)
    b:set_u8(3, 0x00)
    if off < #grb then
      b:set_bytes(4, grb:sub(off + 1, math.min(#grb, off + 60)))
    end
    dev.transport:write(b)
  end
  dev.transport:write(string.char(0x22, 0xA0, ch, 0x00,
    0x01, 0x00, 0x00, 0x28, 0x00, 0x00, 0x80, 0x00, 0x32, 0x00, 0x00, 0x01))
end

local function write_logo(dev, color)
  local b = halod.buffer(64)
  b:set_u8(0, 0x2A)
  b:set_u8(1, 0x04)
  b:set_u8(2, 0x04)
  b:set_u8(3, 0x04)
  b:set_u8(4, 0x00)
  b:set_u8(5, 0x32)
  b:set_u8(6, 0x00)
  b:set_u8(7, color.g)
  b:set_u8(8, color.r)
  b:set_u8(9, color.b)
  b:set_u8(56, 0x01)
  b:set_u8(57, 0x00)
  b:set_u8(58, 0x01)
  b:set_u8(59, 0x03)
  dev.transport:write(b)
end

local RING_LEDS = 8

-- 8-LED ring starting at ~1:30 (top-right, offset by π/4 like native), plus
-- the single logo LED at center.
local function ring_zone()
  local l = {}
  for i = 0, RING_LEDS - 1 do
    local angle = (2 * math.pi * i / RING_LEDS) - (math.pi / 2) + (math.pi / 4)
    l[#l + 1] = { id = i, x = 0.5 + 0.42 * math.cos(angle), y = 0.5 + 0.42 * math.sin(angle) }
  end
  return l
end

return {
  match = {
    { transport = "hid", vid = 0x1E71, pid = 0x2007, name = "Kraken X53/X63/X73", device_type = "a_i_o" },
    { transport = "hid", vid = 0x1E71, pid = 0x2014, name = "Kraken X53/X63/X73", device_type = "a_i_o" },
  },
  identity = { vendor = "NZXT", model = "Kraken X" },
  transports = { hid = { report_size = REPORT, timeout_ms = 1000 } },

  rgb = {
    zones = {
      { id = "ring", name = "Ring", topology = { type = "ring" }, leds = ring_zone() },
      { id = "logo", name = "Logo", topology = { type = "linear" },
        leds = { { id = 0, x = 0.5, y = 0.5 } } },
    },
  },
  sensor = {},
  poll = { interval_ms = 500 },

  -- RGB-only external accessory header (no software fan-duty control on this
  -- wire family — `fan = false` keeps the child accessory's speed uncontrollable).
  chain = {
    channels = { { id = "0", name = "Aer/F Fan", max_leds = 40 } },
    accessories = {
      { id = 0x13, name = "F120 RGB", led_count = 8, topology = "ring", fan = false },
      { id = 0x14, name = "F140 RGB", led_count = 8, topology = "ring", fan = false },
      { id = 0x17, name = "F140 RGB Core", led_count = 8, topology = "ring", fan = false },
      { id = 0x18, name = "F140 RGB Core", led_count = 8, topology = "ring", fan = false },
      { id = 0x1B, name = "F240 RGB Core", led_count = 16, topology = "rings", rings = 2, fan = false },
      { id = 0x1C, name = "F240 RGB Core", led_count = 16, topology = "rings", rings = 2, fan = false },
      { id = 0x1D, name = "F360 RGB Core", led_count = 24, topology = "rings", rings = 3, fan = false },
      { id = 0x1E, name = "F360 RGB Core", led_count = 24, topology = "rings", rings = 3, fan = false },
      { id = 0x1F, name = "F420 RGB Core", led_count = 24, topology = "rings", rings = 3, fan = false },
    },
  },

  initialize = function(dev)
    dev.transport:write(string.char(0x70, 0x02, 0x01, 0xB8, 0x01)) -- INIT_SET
    dev.transport:write(string.char(0x70, 0x01))                   -- firmware push
    dev.transport:write(string.char(0x10, 0x01))                   -- enable status stream
    log("NZXT Kraken X initialized")
    return { ok = true }
  end,

  write_frame = function(dev, zone_id, colors)
    if zone_id == "logo" then
      local c = colors[1] or { r = 0, g = 0, b = 0 }
      write_logo(dev, c)
    else
      write_x3_channel(dev, 0x02, colors)
    end
  end,
  apply = function(dev, state)
    if state.mode == "static" then
      local fill = {}
      for i = 1, RING_LEDS do fill[i] = state.color end
      write_x3_channel(dev, 0x02, fill)
      write_logo(dev, state.color)
    elseif state.mode == "per_led" then
      local zones = state.zones or {}
      local ring_map = zones.ring or {}
      local fill = {}
      for i = 0, RING_LEDS - 1 do
        fill[i + 1] = ring_map[tostring(i)] or { r = 0, g = 0, b = 0 }
      end
      write_x3_channel(dev, 0x02, fill)
      local logo_map = zones.logo or {}
      write_logo(dev, logo_map["0"] or { r = 0, g = 0, b = 0 })
    end
  end,

  -- Accessory (F-fan) RGB.
  write_ext_frame = function(dev, channel, colors)
    write_x3_channel(dev, 0x01, colors)
  end,

  -- Status stream (0x75): liquid temp only — pump/fan duty aren't
  -- software-controllable on this wire family, so nothing else is surfaced.
  read_status = function(dev)
    local r = halod.buffer(dev.transport:read_nonblocking(REPORT))
    if #r < 26 or r:get_u8(0) ~= 0x75 then
      return dev.status
    end
    local frac = r:get_u8(16)
    if frac > 9 then frac = 9 end
    return { liquid_temp = r:get_u8(15) + frac / 10.0 }
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
