-- SPDX-License-Identifier: GPL-3.0-or-later
-- SPDX-FileCopyrightText: Aleksandr Mezin <mezin.alexander@gmail.com>
--
-- NZXT RGB & Fan Controller ("Control Hub") built-in plugin for HaloDaemon —
-- a 5-channel RGB + fan hub with no LEDs or sensors of its own; every zone
-- and fan lives on a chained accessory (F-fan). This is the only driver for
-- this PID (there is no native Rust fallback); disabling it makes the device
-- undetected.
--
-- Protocol reference: the Linux kernel `nzxt-smart2` hwmon driver by
-- Aleksandr Mezin (GPL-2.0-or-later). Verified offsets: firmware/accessory
-- handshake shared with Kraken (0x10/0x20); lighting 0x26 0x04 (data) / 0x26
-- 0x06 (commit); fan status push 0x67 0x02 (rpm/duty), fan config push 0x61
-- 0x03 (fan type); set fan duty 0x62 0x01.

local REPORT = 64
local FAN_CHANNELS = 5
local MAX_CHAIN_LEDS = 96

-- One 0x26 0x04 GRB data packet (unpadded) plus a 64-byte 0x26 0x06 commit,
-- for the given channel's bitmask.
local function write_channel(dev, channel, colors)
  local ch_byte = 1 << channel
  local parts = {}
  for _, c in ipairs(colors) do
    parts[#parts + 1] = string.char(c.g, c.r, c.b)
  end
  local grb = table.concat(parts)

  local data = halod.buffer(4 + #grb)
  data:set_u8(0, 0x26)
  data:set_u8(1, 0x04)
  data:set_u8(2, ch_byte)
  data:set_u8(3, 0x00)
  data:set_bytes(4, grb)
  dev.transport:write(data)

  local commit = halod.buffer(64)
  commit:set_u8(0, 0x26)
  commit:set_u8(1, 0x06)
  commit:set_u8(2, ch_byte)
  commit:set_u8(3, 0x00)
  commit:set_bytes(4, string.char(0x01, 0x00, 0x00, 0x18, 0x00, 0x00, 0x80, 0x00, 0x32, 0x00, 0x00, 0x01))
  dev.transport:write(commit)
end

local chain_channels = {}
for i = 0, FAN_CHANNELS - 1 do
  chain_channels[#chain_channels + 1] =
    { id = tostring(i), name = "Channel " .. (i + 1), max_leds = MAX_CHAIN_LEDS }
end

return {
  match = {
    { transport = "hid", vid = 0x1E71, pid = 0x2022, name = "Control Hub", device_type = "hub" },
  },
  identity = { vendor = "NZXT", model = "Control Hub" },
  transports = { hid = { report_size = REPORT, timeout_ms = 1000 } },
  poll = { interval_ms = 1000 },

  chain = {
    channels = chain_channels,
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
    -- Configure the hardware's own status-push interval to ~1000ms (control
    -- byte 3: 488 + (3-1)*256), matching the `poll` cadence above.
    dev.transport:write(string.char(0x60, 0x02, 0x01, 0xE8, 0x03, 0x01, 0xE8, 0x03))
    dev.transport:write(string.char(0x60, 0x03)) -- detect_fans: triggers a fan-config push
    log("NZXT Control Hub initialized")
    return { ok = true }
  end,

  -- RGB, routed by the host through whichever channel a chained accessory occupies.
  write_ext_frame = function(dev, channel, colors)
    write_channel(dev, tonumber(channel), colors)
  end,

  -- Status pushes: 0x67 0x02 (rpm+duty+fan_type for all 5 channels) or 0x61
  -- 0x03 (fan_type only, e.g. right after detect_fans). A 0x61 push updates
  -- only fan_type, keeping the last known rpm/duty.
  read_status = function(dev)
    local prev = dev.status or { rpm = {}, duty = {}, fan_type = {} }
    local r = halod.buffer(dev.transport:read_nonblocking(REPORT))
    if #r < 2 then return prev end
    if r:get_u8(0) == 0x67 and r:get_u8(1) == 0x02 and #r >= 45 then
      local rpm, duty, fan_type = {}, {}, {}
      for i = 0, FAN_CHANNELS - 1 do
        rpm[i] = r:get_u16_le(24 + i * 2)
        duty[i] = r:get_u8(40 + i)
        fan_type[i] = r:get_u8(16 + i)
      end
      return { rpm = rpm, duty = duty, fan_type = fan_type }
    elseif r:get_u8(0) == 0x61 and r:get_u8(1) == 0x03 and #r >= 21 then
      local fan_type = {}
      for i = 0, FAN_CHANNELS - 1 do fan_type[i] = r:get_u8(16 + i) end
      return { rpm = prev.rpm, duty = prev.duty, fan_type = fan_type }
    end
    return prev
  end,

  -- Per-channel fan telemetry/control, routed from the chained accessory's
  -- fan capability via the parent hub.
  fan_rpm = function(dev, ch)
    local s = dev.status or {}
    return (s.rpm and s.rpm[ch]) or 0
  end,
  fan_duty = function(dev, ch)
    local s = dev.status or {}
    return (s.duty and s.duty[ch]) or 0
  end,
  fan_controllable = function(dev, ch)
    local s = dev.status or {}
    local t = s.fan_type and s.fan_type[ch]
    return t ~= nil and t ~= 0
  end,
  set_fan_duty = function(dev, ch, duty)
    if ch >= FAN_CHANNELS then
      error(string.format("Control Hub: fan channel %d out of range (max %d)", ch, FAN_CHANNELS - 1))
    end
    local pkt = halod.buffer(11)
    pkt:set_u8(0, 0x62)
    pkt:set_u8(1, 0x01)
    pkt:set_u8(2, 1 << ch)
    pkt:set_u8(3 + ch, duty)
    dev.transport:write(pkt)
  end,

  -- Accessory detection (0x20 0x03 -> 0x21 0x03): up to 8 channels, first
  -- non-zero accessory id per channel (byte 15 + channel*6).
  detect_accessories = function(dev)
    dev.transport:write(string.char(0x20, 0x03))
    for _ = 1, 16 do
      local ok, s = pcall(function() return dev.transport:read(REPORT) end)
      if not ok then return {} end
      local r = halod.buffer(s)
      if #r >= 2 and r:get_u8(0) == 0x21 and r:get_u8(1) == 0x03 then
        local count = math.min(#r > 14 and r:get_u8(14) or 0, 8)
        local out = {}
        for ch = 0, count - 1 do
          local offset = 15 + ch * 6
          if offset < #r then
            local acc = r:get_u8(offset)
            if acc ~= 0 then
              out[#out + 1] = { channel = ch, accessory = acc }
            end
          end
        end
        return out
      end
    end
    return {}
  end,
}
