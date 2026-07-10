-- SPDX-License-Identifier: GPL-2.0-or-later
-- SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project
-- Reference: OpenRGB Zotac GPU controller (SPECTRA Blackwell)
--
-- Zotac SPECTRA 2.0 (Blackwell) GPU RGB, ported from the native Rust driver to a
-- built-in Lua plugin. The controller sits on the GPU's I²C bus at 0x4B; the PCI
-- gate below confines the scan to Zotac cards (the GPU segment is shared with the
-- display's DDC/EDID lines). Register staging is paced by the declared
-- `max_bytes_per_sec` — the native driver sleeps 3ms between register writes, so
-- ~333 B/s reproduces that timing through the metered bus.

local ZOTAC_ADDR = 0x4B

local REG_BASE   = 0x20 -- staging registers 0x20..0x2F
local REG_DETECT = 0x10
local REG_COMMIT = 0x17
local COMMIT_VAL = 0x01

local FULL_BRIGHTNESS = 100
local DEFAULT_SPEED   = 50

-- Effect mode ids (register 0x22).
local MODE = {
  static = 0x01, breathe = 0x02, fade = 0x03, wink = 0x04, glide = 0x08,
  prism = 0x09, bokeh = 0x0A, beacon = 0x0B, tandem = 0x18, tidal = 0x19,
  astra = 0x20, cosmic = 0x21, volta = 0x22,
}
local DIRECTION = { left = 0x00, right = 0x01 }

-- Fixed hardware zones (each a single LED).
local ZONES = {
  { id = "logo", name = "Logo", index = 0 },
  { id = "side_bar", name = "Side Bar", index = 1 },
  { id = "infinity_mirror", name = "Infinity Mirror", index = 2 },
}

-- PCI gate: NVIDIA silicon (0x10DE) with Zotac's subsystem vendor (0x19DA).
-- confirmed = false ⇒ `initialize`'s detect read validates before use.
local GPU_PCI_MATCH = {
  { vendor = 0x10DE, sub_vendor = 0x19DA, confirmed = false },
}

-- ── Register staging ─────────────────────────────────────────────────────────

-- 16 staging registers 0x20..0x2F for one zone (index i → register 0x20 + i).
local function frame_regs(zone, mode, c1, c2, brightness, speed, direction)
  return {
    0x00, zone, mode,
    c1.r, c1.g, c1.b,
    c2.r, c2.g, c2.b,
    brightness, speed, direction,
    0, 0, 0, 0,
  }
end

-- Bus pacing (max_bytes_per_sec) stands in for the native per-write sleep.
local function stage_and_commit(ops, addr, regs)
  for i = 1, 16 do
    ops:write_byte_data(addr, REG_BASE + (i - 1), regs[i])
  end
  ops:write_byte_data(addr, REG_COMMIT, COMMIT_VAL)
end

local BLACK = { r = 0, g = 0, b = 0 }

-- Write one Mode::Static frame per zone from `colors_by_index` (index → color).
local function write_static(ops, addr, colors_by_index)
  for _, z in ipairs(ZONES) do
    local c = colors_by_index[z.index] or BLACK
    stage_and_commit(ops, addr,
      frame_regs(z.index, MODE.static, c, BLACK, FULL_BRIGHTNESS, 0, DIRECTION.left))
  end
end

local function clamp_speed(v)
  if type(v) ~= "number" then return DEFAULT_SPEED end
  if v < 0 then return 0 elseif v > 100 then return 100 end
  return math.floor(v + 0.5)
end

-- ── Effect param descriptors ─────────────────────────────────────────────────

local COLOR_PARAM = { id = "color", label = "Color", kind = { kind = "color" }, default = { r = 255, g = 0, b = 0 } }
local SPEED_PARAM = { id = "speed", label = "Speed", kind = { kind = "range", min = 0, max = 100, step = 1 }, default = DEFAULT_SPEED }
local DIR_PARAM   = { id = "direction", label = "Direction", kind = { kind = "enum", options = { "left", "right" } }, default = "left" }

-- id, hardware mode, and which params each effect exposes.
local EFFECT_DEFS = {
  { id = "breathe", name = "Breathe", color = true, direction = false },
  { id = "fade", name = "Fade", color = false, direction = false },
  { id = "wink", name = "Wink", color = true, direction = false },
  { id = "glide", name = "Glide", color = true, direction = true },
  { id = "prism", name = "Prism", color = false, direction = true },
  { id = "bokeh", name = "Bokeh", color = true, direction = false },
  { id = "beacon", name = "Beacon", color = true, direction = false },
  { id = "tandem", name = "Tandem", color = true, direction = false },
  { id = "tidal", name = "Tidal", color = true, direction = true },
  { id = "astra", name = "Astra", color = true, direction = false },
  { id = "cosmic", name = "Cosmic", color = true, direction = false },
  { id = "volta", name = "Volta", color = true, direction = false },
}

local NATIVE_EFFECTS = {}
for _, e in ipairs(EFFECT_DEFS) do
  local params = {}
  if e.color then params[#params + 1] = COLOR_PARAM end
  params[#params + 1] = SPEED_PARAM
  if e.direction then params[#params + 1] = DIR_PARAM end
  NATIVE_EFFECTS[#NATIVE_EFFECTS + 1] = { id = e.id, name = e.name, params = params }
end

-- ── Plugin ───────────────────────────────────────────────────────────────────

return {
  match = {
    transport = "smbus", bus = "gpu",
    addresses = { ZOTAC_ADDR }, probe = "none",
    pci_match = GPU_PCI_MATCH,
    max_bytes_per_sec = 333,
    name = "Zotac SPECTRA GPU RGB", device_type = "gpu",
  },
  identity = {
    vendor = "Zotac", model = "Blackwell SPECTRA 2.0", id = "zotac_spectra_gpu",
    author = "HaloDaemon", version = "1.0.0",
    description = "Zotac SPECTRA 2.0 (Blackwell) GPU RGB over the GPU I²C bus.",
  },
  rgb = { zones = {}, native_effects = NATIVE_EFFECTS },

  initialize = function(dev)
    local addr = dev.match.addr
    local present = dev.transport:batch(function(ops)
      return ops:read_byte_data(addr, REG_DETECT) ~= nil
    end)
    if not present then return { ok = false } end
    local zones = {}
    for _, z in ipairs(ZONES) do
      zones[#zones + 1] = { id = z.id, name = z.name, topology = "linear", led_count = 1 }
    end
    return { ok = true, model = "Blackwell SPECTRA 2.0", zones = zones }
  end,

  apply = function(dev, state)
    local addr = dev.match.addr
    dev.transport:batch(function(ops)
      if state.mode == "static" then
        local by_index = {}
        for _, z in ipairs(ZONES) do by_index[z.index] = state.color end
        write_static(ops, addr, by_index)
      elseif state.mode == "per_led" then
        local zones = state.zones or {}
        local by_index = {}
        for _, z in ipairs(ZONES) do
          local m = zones[z.id]
          by_index[z.index] = (m and m["0"]) or BLACK
        end
        write_static(ops, addr, by_index)
      elseif state.mode == "native_effect" then
        local mode = MODE[state.id]
        if mode then
          local params = state.params or {}
          local c1 = params.color or { r = 255, g = 0, b = 0 }
          local speed = clamp_speed(params.speed)
          local direction = DIRECTION[params.direction] or DIRECTION.left
          for _, z in ipairs(ZONES) do
            stage_and_commit(ops, addr,
              frame_regs(z.index, mode, c1, BLACK, FULL_BRIGHTNESS, speed, direction))
          end
        end
      end
      return true
    end)
  end,

  -- Canvas-engine frame: one zone, static color.
  write_frame = function(dev, zone_id, colors)
    local addr = dev.match.addr
    local index
    for _, z in ipairs(ZONES) do
      if z.id == zone_id then index = z.index break end
    end
    if index == nil then return end
    local c = colors[1] or BLACK
    dev.transport:batch(function(ops)
      stage_and_commit(ops, addr,
        frame_regs(index, MODE.static, c, BLACK, FULL_BRIGHTNESS, 0, DIRECTION.left))
      return true
    end)
  end,
}
