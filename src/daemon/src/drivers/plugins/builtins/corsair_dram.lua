-- SPDX-License-Identifier: GPL-2.0-or-later
-- SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project
-- SPDX-FileCopyrightText: Erik Gilling (konkers) — OpenRGB project
-- Reference: OpenRGB Corsair DRAM controller
-- https://gitlab.com/CalcProgrammer1/OpenRGB/-/tree/master/Controllers/CorsairDRAMController
--
-- Corsair iCUE DRAM RGB (DDR4/DDR5), ported from the native Rust driver to a
-- built-in Lua plugin. All register I/O runs inside `dev.transport:batch(fn)`,
-- which holds the i2c bus lock across the callback.

-- ── Register map ─────────────────────────────────────────────────────────────
local REG_RESET_BUFFER        = 0x0B
local REG_SET_BINARY_DATA     = 0x20
local REG_BINARY_START        = 0x21
local REG_STATUS              = 0x30
local REG_COLOR_BUFFER_BLOCK_1 = 0x31
local REG_COLOR_BUFFER_BLOCK_2 = 0x32
local REG_GET_BINARY_DATA     = 0x40
local REG_GET_CHECKSUM        = 0x42
local REG_SENTINEL_A          = 0x43
local REG_SENTINEL_B          = 0x44
local REG_GET_DEVICE_INFO     = 0x61
local REG_WRITE_CONFIGURATION = 0x82

local CONFIG_ID_EFFECT     = 0x01
local CONFIG_ID_COLOR_DATA = 0x02
local STATUS_BUSY_BIT      = 0x08
local CORSAIR_DRAM_VID     = 0x1B1C

local DRAM_ADDRESSES = {
  0x58, 0x59, 0x5A, 0x5B, 0x5C, 0x5D, 0x5E, 0x5F,
  0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
}

-- ── Native effect modes / enums ──────────────────────────────────────────────
local MODE = { color_shift = 0x00, breathing = 0x01, rainbow_wave = 0x03 }
local SPEED = { slow = 0, medium = 1, fast = 2 }
local DIRECTION = { up = 0, down = 1, left = 2, right = 3 }

-- ── PID → (name, led_count, reverse) table ───────────────────────────────────
local DEVICE_TABLE = {
  { pids = { 0x0700, 0x0701, 0x0900, 0x0901, 0x0910, 0x0911 }, name = "Corsair Vengeance RGB DDR5", led_count = 10, reverse = false },
  { pids = { 0x0600, 0x0601 }, name = "Corsair Dominator Platinum RGB DDR5", led_count = 12, reverse = true },
  { pids = { 0x0800, 0x0801, 0x0810, 0x0811 }, name = "Corsair Dominator Titanium RGB DDR5", led_count = 12, reverse = true },
  { pids = { 0x0A00, 0x0A01, 0x0A10, 0x0A11 }, name = "Corsair Vengeance Shugo Series DDR5", led_count = 10, reverse = false },
  { pids = { 0x0B00, 0x0B01 }, name = "Corsair Vengeance RGB RS DDR5", led_count = 6, reverse = false },
  { pids = { 0x0100, 0x0101 }, name = "Corsair Vengeance RGB Pro DDR4", led_count = 10, reverse = false },
  { pids = { 0x0200, 0x0201 }, name = "Corsair Dominator Platinum RGB DDR4", led_count = 12, reverse = true },
  { pids = { 0x0300, 0x0301 }, name = "Corsair Vengeance RGB Pro SL DDR4", led_count = 10, reverse = false },
  { pids = { 0x0400, 0x0401 }, name = "Corsair Vengeance RGB RS DDR4", led_count = 6, reverse = false },
}

local function device_from_pid(pid)
  for _, e in ipairs(DEVICE_TABLE) do
    for _, p in ipairs(e.pids) do
      if p == pid then return e.name, e.led_count, e.reverse end
    end
  end
  return "Corsair DRAM RGB", 10, false
end

-- ── CRC-8/SMBus (poly 0x07, init 0x00) ───────────────────────────────────────
local function crc8(data)
  local crc = 0
  for i = 1, #data do
    crc = (crc ~ data[i]) & 0xFF
    for _ = 1, 8 do
      if crc & 0x80 ~= 0 then
        crc = ((crc << 1) ~ 0x07) & 0xFF
      else
        crc = (crc << 1) & 0xFF
      end
    end
  end
  return crc
end

-- ── Detection ────────────────────────────────────────────────────────────────
local function probe(ops, addr)
  if not ops:write_quick(addr) then return false end
  local a = ops:read_byte_data(addr, REG_SENTINEL_A)
  if a ~= 0x1A and a ~= 0x1B and a ~= 0x1C then return false end
  local b = ops:read_byte_data(addr, REG_SENTINEL_B)
  if b ~= 0x01 and b ~= 0x03 and b ~= 0x04 then return false end
  return true
end

-- ── Device info read (32-byte binary block + CRC) ────────────────────────────
local function read_info(ops, addr)
  if not ops:write_byte_data(addr, REG_GET_DEVICE_INFO, 0x00) then return nil end
  if not ops:write_byte_data(addr, REG_BINARY_START, 0x00) then return nil end
  local data = {}
  for i = 1, 32 do
    local v = ops:read_byte_data(addr, REG_GET_BINARY_DATA)
    if v == nil then return nil end
    data[i] = v
  end
  local device_crc = ops:read_byte_data(addr, REG_GET_CHECKSUM)
  if device_crc == nil or crc8(data) ~= device_crc then return nil end

  local vid = data[1] | (data[2] << 8)
  if vid ~= CORSAIR_DRAM_VID then return nil end
  local pid = data[3] | (data[4] << 8)
  local protocol_version = data[29]
  local name, led_count, reverse = device_from_pid(pid)
  return {
    pid = pid,
    led_count = led_count,
    reverse = reverse,
    model = name,
    protocol_version = protocol_version,
  }
end

-- ── Colour packing (reverse-aware) ───────────────────────────────────────────
local function color_at(colors, info, led_idx)
  local idx = info.reverse and (info.led_count - 1 - led_idx) or led_idx
  return colors[idx + 1] or { r = 0, g = 0, b = 0 }
end

-- Direct-mode packet: [led_count, R0,G0,B0, …, CRC8(packet[1..n-1])].
local function build_direct_packet(info, colors)
  local packet = { info.led_count }
  for led = 0, info.led_count - 1 do
    local c = color_at(colors, info, led)
    packet[#packet + 1] = c.r
    packet[#packet + 1] = c.g
    packet[#packet + 1] = c.b
  end
  packet[#packet + 1] = crc8(packet)
  return packet
end

-- Legacy DDR4 effect-mode buffer: R,G,B,0xFF per LED.
local function build_effect_color_data(info, colors)
  local buf = {}
  for led = 0, info.led_count - 1 do
    local c = color_at(colors, info, led)
    buf[#buf + 1] = c.r
    buf[#buf + 1] = c.g
    buf[#buf + 1] = c.b
    buf[#buf + 1] = 0xFF
  end
  return buf
end

local function bytes_to_str(t, from, to)
  local chunk = {}
  for i = from, to do chunk[#chunk + 1] = t[i] end
  return string.char(table.unpack(chunk))
end

local function set_colors_direct(ops, addr, packet)
  local n = #packet
  ops:write_block_data(addr, REG_COLOR_BUFFER_BLOCK_1, bytes_to_str(packet, 1, math.min(n, 32)))
  if n > 32 then
    ops:write_block_data(addr, REG_COLOR_BUFFER_BLOCK_2, bytes_to_str(packet, 33, n))
  end
end

-- ── Binary streaming (effect + legacy colour paths) ──────────────────────────
local function stream_binary(ops, addr, data)
  ops:write_byte_data(addr, REG_RESET_BUFFER, 0x00)
  ops:write_byte_data(addr, REG_BINARY_START, 0x00)
  for i = 1, #data do
    ops:write_byte_data(addr, REG_SET_BINARY_DATA, data[i])
  end
end

-- The register bus has no sleep; poll STATUS a few times and proceed either way
-- (the native wait_ready is best-effort: it ignores errors and gives up after 5).
local function wait_ready(ops, addr)
  for _ = 1, 5 do
    local status = ops:read_byte_data(addr, REG_STATUS)
    if status ~= nil and (status & STATUS_BUSY_BIT) == 0 then return end
  end
end

local function stream_and_commit(ops, addr, data, config_id)
  stream_binary(ops, addr, data)
  if crc8(data) == ops:read_byte_data(addr, REG_GET_CHECKSUM) then
    ops:write_byte_data(addr, REG_WRITE_CONFIGURATION, config_id)
    wait_ready(ops, addr)
  end
end

-- 20-byte native-effect descriptor.
local function build_native_effect(mode, speed, direction, c1, c2, brightness)
  local e = {}
  for i = 1, 20 do e[i] = 0 end
  e[1] = mode
  e[2] = speed
  e[3] = 0x01 -- random = false
  e[4] = direction
  e[5], e[6], e[7] = c1.r, c1.g, c1.b
  e[8] = brightness
  e[9], e[10], e[11] = c2.r, c2.g, c2.b
  e[12] = brightness
  return e
end

local function write_colors(ops, addr, info, colors)
  if info.protocol_version >= 4 then
    set_colors_direct(ops, addr, build_direct_packet(info, colors))
  else
    stream_and_commit(ops, addr, build_effect_color_data(info, colors), CONFIG_ID_COLOR_DATA)
  end
end

-- ── Effect param descriptors ─────────────────────────────────────────────────
local SPEED_ENUM = {
  id = "speed", label = "Speed",
  kind = { kind = "enum", options = { "slow", "medium", "fast" } },
  default = "medium",
}

local NATIVE_EFFECTS = {
  {
    id = "breathing", name = "Breathing",
    params = {
      { id = "color", label = "Color", kind = { kind = "color" }, default = { r = 0, g = 128, b = 255 } },
      SPEED_ENUM,
    },
  },
  {
    id = "rainbow_wave", name = "Rainbow Wave",
    params = {
      SPEED_ENUM,
      { id = "direction", label = "Direction",
        kind = { kind = "enum", options = { "up", "down", "left", "right" } }, default = "right" },
    },
  },
  {
    id = "color_shift", name = "Color Shift",
    params = {
      { id = "color1", label = "Color 1", kind = { kind = "color" }, default = { r = 255, g = 0, b = 0 } },
      { id = "color2", label = "Color 2", kind = { kind = "color" }, default = { r = 0, g = 0, b = 255 } },
      SPEED_ENUM,
    },
  },
  { id = "off", name = "Off", params = {} },
}

-- ── Plugin ───────────────────────────────────────────────────────────────────
return {
  match = {
    transport = "smbus", bus = "chipset",
    addresses = DRAM_ADDRESSES, probe = "quick",
    name = "Corsair DRAM RGB", device_type = "ram",
  },
  identity = {
    vendor = "Corsair", model = "iCUE DRAM", id = "corsair_dram",
    author = "HaloDaemon", version = "1.0.0",
    description = "Corsair iCUE DRAM RGB (Vengeance / Dominator, DDR4 & DDR5).",
  },
  rgb = { zones = {}, native_effects = NATIVE_EFFECTS },

  initialize = function(dev)
    local addr = dev.match.addr
    local info = dev.transport:batch(function(ops)
      if not probe(ops, addr) then return nil end
      return read_info(ops, addr)
    end)
    if not info then return { ok = false } end
    dev.info = info
    return {
      ok = true,
      model = info.model,
      zones = { { id = "leds", name = "LEDs", topology = "linear", led_count = info.led_count } },
    }
  end,

  apply = function(dev, state)
    local info = dev.info
    if not info then error("Corsair DRAM device used before initialize()") end
    local addr = dev.match.addr
    dev.transport:batch(function(ops)
      if state.mode == "static" then
        local colors = {}
        for i = 1, info.led_count do colors[i] = state.color end
        write_colors(ops, addr, info, colors)
      elseif state.mode == "per_led" then
        local zone = state.zones and state.zones["leds"]
        if zone then
          local frame = {}
          for i = 0, info.led_count - 1 do
            frame[i + 1] = zone[tostring(i)] or { r = 0, g = 0, b = 0 }
          end
          write_colors(ops, addr, info, frame)
        end
      elseif state.mode == "native_effect" then
        local params = state.params or {}
        local speed = SPEED[params.speed] or SPEED.medium
        if state.id == "off" then
          local colors = {}
          for i = 1, info.led_count do colors[i] = { r = 0, g = 0, b = 0 } end
          write_colors(ops, addr, info, colors)
        else
          local mode = MODE[state.id]
          if mode then
            local direction = DIRECTION[params.direction] or DIRECTION.right
            local c1 = params.color1 or params.color or { r = 255, g = 255, b = 255 }
            local c2 = params.color2 or { r = 255, g = 255, b = 255 }
            stream_and_commit(ops, addr,
              build_native_effect(mode, speed, direction, c1, c2, 255), CONFIG_ID_EFFECT)
          end
        end
      end
      return true
    end)
  end,

  -- Canvas-engine frame: always the direct block path (fastest, version-agnostic).
  write_frame = function(dev, _zone, colors)
    local info = dev.info
    if not info then error("Corsair DRAM device used before initialize()") end
    local addr = dev.match.addr
    dev.transport:batch(function(ops)
      set_colors_direct(ops, addr, build_direct_packet(info, colors))
      return true
    end)
  end,
}
