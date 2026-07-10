-- SPDX-License-Identifier: GPL-2.0-or-later
-- SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project
-- Reference: OpenRGB ENE SMBus implementation
-- https://gitlab.com/CalcProgrammer1/OpenRGB/-/blob/master/Controllers/ENESMBusController/ENESMBusInterface/ENESMBusInterface_i2c_smbus.cpp
--
-- ASUS Aura / ENE SMBus RGB (DRAM + GPU), ported from the native Rust driver to
-- a built-in Lua plugin. Register I/O runs through `dev.transport:batch(fn)`,
-- which holds the i2c bus lock across the whole callback — the atomicity the
-- direct-mode color writes require.

-- ── Registers ────────────────────────────────────────────────────────────────
local REG_DEVICE_NAME    = 0x1000
local REG_MICRON_CHECK   = 0x1030
local REG_CONFIG_TABLE   = 0x1C00
local REG_COLORS_DIRECT  = 0x8000
local REG_COLORS_EFFECT  = 0x8010
local REG_DIRECT         = 0x8020
local REG_MODE           = 0x8021
local REG_SPEED          = 0x8022
local REG_DIRECTION      = 0x8023
local REG_APPLY          = 0x80A0
local REG_SLOT_INDEX     = 0x80F8
local REG_I2C_ADDRESS    = 0x80F9
local REG_COLORS_DIRECT_V2 = 0x8100
local REG_COLORS_EFFECT_V2 = 0x8160

local APPLY_VAL      = 0x01
local DRAM_BROADCAST = 0x77

local MODE_OFF                = 0
local MODE_STATIC             = 1
local MODE_BREATHING          = 2
local MODE_SPECTRUM_CYCLE_WAVE = 11

local SPEED = { fastest = 0, fast = 1, normal = 2, slow = 3, slowest = 4 }

-- ENE DRAM stick candidate addresses (also the chipset scan set).
local RAM_ADDRESSES = {
  0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x4F,
  0x66, 0x67, 0x39, 0x3A, 0x3B, 0x3C, 0x3D,
}
local GPU_ADDRESS = 0x67

-- ── Low-level register helpers (run inside a batch callback) ─────────────────

-- ENE two-stage addressing: byte-swap the 16-bit register into command 0x00,
-- then read/write via command 0x81 / 0x01.
local function reg_addr_bytes(reg)
  return ((reg << 8) & 0xFF00) | ((reg >> 8) & 0x00FF)
end

local function read_reg(ops, addr, reg)
  if not ops:write_word_data(addr, 0x00, reg_addr_bytes(reg)) then return nil end
  return ops:read_byte_data(addr, 0x81)
end

local function write_reg(ops, addr, reg, val)
  assert(ops:write_word_data(addr, 0x00, reg_addr_bytes(reg)))
  assert(ops:write_byte_data(addr, 0x01, val))
end

-- ENE wire order is R, B, G (green/blue swapped); pad/truncate to led_count*3.
local function build_color_buffer(colors, led_count)
  local buf = {}
  for i = 1, led_count do
    local c = colors[i]
    if c then
      buf[#buf + 1] = c.r
      buf[#buf + 1] = c.b
      buf[#buf + 1] = c.g
    else
      buf[#buf + 1] = 0
      buf[#buf + 1] = 0
      buf[#buf + 1] = 0
    end
  end
  return buf
end

local MAX_BLOCK = 32

-- Block write (cmd 0x03) auto-increments the register pointer; falls back to
-- byte-at-a-time when the controller/bus rejects block transfers.
local function write_reg_block(ops, addr, reg, data)
  local len = #data
  local offset = 0
  while offset < len do
    local chunk_end = math.min(offset + MAX_BLOCK, len)
    local chunk = {}
    for i = offset + 1, chunk_end do chunk[#chunk + 1] = data[i] end
    assert(ops:write_word_data(addr, 0x00, reg_addr_bytes(reg + offset)))
    if ops:write_block_data(addr, 0x03, string.char(table.unpack(chunk))) then
      offset = chunk_end
    else
      for j = 0, #chunk - 1 do
        local full_reg = reg + offset + j
        assert(ops:write_word_data(addr, 0x00, reg_addr_bytes(full_reg)))
        assert(ops:write_byte_data(addr, 0x01, chunk[j + 1]))
      end
      offset = chunk_end
    end
  end
end

-- Direct-mode color write with the full recovery preamble: some controllers only
-- latch direct writes while DIRECT is already high, so assert it before the block.
local function apply_direct_color_block(ops, addr, direct_reg, buf)
  write_reg(ops, addr, REG_MODE, MODE_STATIC)
  write_reg(ops, addr, REG_APPLY, APPLY_VAL)
  write_reg(ops, addr, REG_DIRECT, 0x01)
  write_reg(ops, addr, REG_APPLY, APPLY_VAL)
  write_reg_block(ops, addr, direct_reg, buf)
  write_reg(ops, addr, REG_DIRECT, 0x01)
  write_reg(ops, addr, REG_APPLY, APPLY_VAL)
end

local function set_direct_mode(ops, addr, enable)
  if enable then
    write_reg(ops, addr, REG_MODE, MODE_STATIC)
    write_reg(ops, addr, REG_APPLY, APPLY_VAL)
  end
  write_reg(ops, addr, REG_DIRECT, enable and 0x01 or 0x00)
  write_reg(ops, addr, REG_APPLY, APPLY_VAL)
end

local function set_effect_colors(ops, addr, effect_reg, buf)
  write_reg_block(ops, addr, effect_reg, buf)
  write_reg(ops, addr, REG_APPLY, APPLY_VAL)
end

local function set_mode(ops, addr, mode, speed, direction)
  write_reg(ops, addr, REG_MODE, mode)
  write_reg(ops, addr, REG_SPEED, speed)
  write_reg(ops, addr, REG_DIRECTION, direction)
  write_reg(ops, addr, REG_APPLY, APPLY_VAL)
end

-- ── Detection / device info ──────────────────────────────────────────────────

local function probe(ops, addr)
  -- Quick presence.
  if ops:read_byte(addr) == nil and ops:read_byte_data(addr, 0x00) == nil then
    return false
  end
  -- Incrementing pattern at 0xA0..0xAF.
  for i = 0, 0x0F do
    if ops:read_byte_data(addr, 0xA0 + i) ~= i then return false end
  end
  -- Reject Micron modules.
  local buf = {}
  for i = 0, 5 do
    local v = read_reg(ops, addr, REG_MICRON_CHECK + i)
    if v == nil then return false end
    buf[i + 1] = v
  end
  if string.char(table.unpack(buf)) == "Micron" then return false end
  return true
end

-- Determine (led_count, direct_reg, effect_reg) from the firmware version.
-- Matches the native `apply_version_layout` exactly.
local function version_layout(version, config)
  local offset, direct_reg, effect_reg
  if version == "LED-0116" or version == "DIMM_LED-0102"
      or version == "DIMM_LED-0103" or version == "AUMA0-E8K4-0101" then
    offset, direct_reg, effect_reg = 0x02, REG_COLORS_DIRECT, REG_COLORS_EFFECT
  elseif version == "AUDA0-E6K5-0101"
      or version == "AUMA0-E6K5-0106" or version == "AUMA0-E6K5-0105"
      or version == "AUMA0-E6K5-0104" then
    offset, direct_reg, effect_reg = 0x02, REG_COLORS_DIRECT_V2, REG_COLORS_EFFECT_V2
  elseif version == "AUMA0-E6K5-0107" or version == "AUMA0-E6K5-1110"
      or version == "AUMA0-E6K5-1111" or version == "AUMA0-E6K5-1107"
      or version == "AUMA0-E6K5-0008" or version == "AUMA0-E6K5-1113"
      or version == "AUMA0-E6K5-1114" then
    -- GPU controllers — LED count at 0x03.
    offset, direct_reg, effect_reg = 0x03, REG_COLORS_DIRECT_V2, REG_COLORS_EFFECT_V2
  else
    offset, direct_reg, effect_reg = 0x02, REG_COLORS_DIRECT, REG_COLORS_EFFECT
  end
  local count = math.max(config[offset] or 0, config[0x03] or 0)
  if count > 30 then count = 30 end
  return count, direct_reg, effect_reg
end

local function build_info(ops, addr)
  local vbytes = {}
  for i = 0, 15 do
    local v = read_reg(ops, addr, REG_DEVICE_NAME + i)
    if v == nil then return nil end
    vbytes[i + 1] = v
  end
  local version = ""
  for i = 1, 16 do
    if vbytes[i] == 0 then break end
    version = version .. string.char(vbytes[i])
  end

  local config = {}
  for i = 0, 63 do
    local v = read_reg(ops, addr, REG_CONFIG_TABLE + i)
    if v == nil then return nil end
    config[i] = v
  end

  local led_count, direct_reg, effect_reg = version_layout(version, config)
  if led_count == 0 then return nil end
  return {
    version = version,
    led_count = led_count,
    direct_reg = direct_reg,
    effect_reg = effect_reg,
  }
end

-- Remap DRAM sticks from broadcast 0x77 to individual candidate addresses.
local function remap_dram(ops)
  local idx = 1
  for slot = 0, 7 do
    if not ops:write_quick(DRAM_BROADCAST) then break end
    local target
    while true do
      if idx > #RAM_ADDRESSES then return end
      local candidate = RAM_ADDRESSES[idx]
      idx = idx + 1
      if not ops:write_quick(candidate) then -- NACK = address is free
        target = candidate
        break
      end
    end
    write_reg(ops, DRAM_BROADCAST, REG_SLOT_INDEX, slot)
    write_reg(ops, DRAM_BROADCAST, REG_I2C_ADDRESS, target << 1)
  end
end

-- ── Effect param descriptors (static) ────────────────────────────────────────

local SPEED_ENUM = {
  id = "speed", label = "Speed",
  kind = { kind = "enum", options = { "fastest", "fast", "normal", "slow", "slowest" } },
  default = "normal",
}

local NATIVE_EFFECTS = {
  {
    id = "breathing", name = "Breathing",
    params = {
      { id = "color", label = "Color", kind = { kind = "color" }, default = { r = 255, g = 0, b = 0 } },
      SPEED_ENUM,
    },
  },
  { id = "spectrum_wave", name = "Spectrum Wave", params = { SPEED_ENUM } },
  { id = "off", name = "Off", params = {} },
}

-- ── Plugin ───────────────────────────────────────────────────────────────────

return {
  match = {
    {
      transport = "smbus", bus = "chipset",
      addresses = RAM_ADDRESSES, extra_addresses = { DRAM_BROADCAST },
      max_bytes_per_sec = 6000, pre_scan = true, probe = "quick",
      name = "ENE DRAM RGB", device_type = "ram",
    },
    {
      transport = "smbus", bus = "gpu",
      addresses = { GPU_ADDRESS }, probe = "quick",
      name = "ASUS GPU RGB", device_type = "gpu",
    },
  },
  identity = { vendor = "ASUS/ENE", model = "ENE SMBus", id = "ene" },
  rgb = { zones = {}, native_effects = NATIVE_EFFECTS },

  -- Broadcast-remap DRAM sticks onto individual addresses before probing.
  pre_scan = function(dev)
    dev.transport:batch(function(ops)
      remap_dram(ops)
      return true
    end)
  end,

  initialize = function(dev)
    local addr = dev.match.addr
    local info = dev.transport:batch(function(ops)
      if not probe(ops, addr) then return nil end
      local built = build_info(ops, addr)
      if not built then return nil end
      set_direct_mode(ops, addr, true)
      return built
    end)
    if not info then return { ok = false } end
    dev.info = info
    return {
      ok = true,
      model = info.version,
      zones = { { id = "leds", name = "LEDs", topology = "linear", led_count = info.led_count } },
    }
  end,

  apply = function(dev, state)
    local info = dev.info
    if not info then error("ENE device used before initialize()") end
    local addr = dev.match.addr
    dev.transport:batch(function(ops)
      if state.mode == "static" then
        local colors = {}
        for i = 1, info.led_count do colors[i] = state.color end
        apply_direct_color_block(ops, addr, info.direct_reg, build_color_buffer(colors, info.led_count))
      elseif state.mode == "per_led" then
        local zone = state.zones and state.zones["leds"]
        if zone then
          local frame = {}
          for i = 0, info.led_count - 1 do
            frame[i + 1] = zone[tostring(i)] or { r = 0, g = 0, b = 0 }
          end
          apply_direct_color_block(ops, addr, info.direct_reg, build_color_buffer(frame, info.led_count))
        end
      elseif state.mode == "native_effect" then
        local params = state.params or {}
        local speed = SPEED[params.speed] or SPEED.normal
        if state.id == "breathing" then
          local color = params.color or { r = 255, g = 0, b = 0 }
          local colors = {}
          for i = 1, info.led_count do colors[i] = color end
          set_direct_mode(ops, addr, false)
          set_effect_colors(ops, addr, info.effect_reg, build_color_buffer(colors, info.led_count))
          set_mode(ops, addr, MODE_BREATHING, speed, 0)
        elseif state.id == "spectrum_wave" then
          set_direct_mode(ops, addr, false)
          set_mode(ops, addr, MODE_SPECTRUM_CYCLE_WAVE, speed, 0)
        elseif state.id == "off" then
          set_mode(ops, addr, MODE_OFF, SPEED.normal, 0)
        end
      end
      -- "engine" / "direct_effect": the canvas engine drives write_frame.
      return true
    end)
  end,

  -- Canvas-engine frame: color data only (device is already in direct mode).
  write_frame = function(dev, _zone, colors)
    local info = dev.info
    if not info then error("ENE device used before initialize()") end
    local addr = dev.match.addr
    dev.transport:batch(function(ops)
      write_reg_block(ops, addr, info.direct_reg, build_color_buffer(colors, info.led_count))
      return true
    end)
  end,
}
