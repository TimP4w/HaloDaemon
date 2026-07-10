-- SPDX-License-Identifier: GPL-3.0-or-later
--
-- Example RGB effects plugin for HaloDaemon.
--
-- Declares two effects, no hardware match — an effect-only plugin (`type =
-- "effect"`) registers into the RGB engine's effect catalog instead of the
-- device-discovery path:
--
--   * "plasma" (pixmap/canvas): fills a shared 400x300 linear-RGBA buffer
--     once per frame; every zone using it then samples the buffer at its
--     LED positions.
--   * "comet" (direct): computes one color per LED directly from its chain
--     position, once per zone per frame.
--
-- Drop this in ~/.config/halod/plugins/ and press "Scan now" (or restart the
-- daemon). Both effects then appear in their respective pickers: "Plasma" in
-- the canvas effect list (Lighting > pick an effect for a zone), "Comet" in
-- a device's Direct Effect list. No permissions are needed — effects are
-- pure compute and never touch hardware.

-- Plain-Lua hue (0..1, s=1, v=1) to sRGB bytes. Used to build the palette
-- below at load time — top-level script code also runs in the throwaway VM
-- `parse_manifest` uses to read the manifest tables, which has no `halod`
-- global, so this can't call the host-provided `halod.hsv` helper (that one
-- is only safe to call from inside a callback, which only ever runs in a
-- worker VM).
local function hue_to_rgb(h)
  local h6 = (h % 1.0) * 6.0
  local i = math.floor(h6)
  local f = h6 - i
  local r, g, b
  if i == 0 then r, g, b = 1.0, f, 0.0
  elseif i == 1 then r, g, b = 1.0 - f, 1.0, 0.0
  elseif i == 2 then r, g, b = 0.0, 1.0, f
  elseif i == 3 then r, g, b = 0.0, 1.0 - f, 1.0
  elseif i == 4 then r, g, b = f, 0.0, 1.0
  else r, g, b = 1.0, 0.0, 1.0 - f
  end
  return math.floor(r * 255.0 + 0.5), math.floor(g * 255.0 + 0.5), math.floor(b * 255.0 + 0.5)
end

-- 256-entry rainbow palette, built once at load time so the plasma render
-- loop below never computes a hue conversion per pixel — 120000 pixels/frame
-- in interpreted Lua adds up fast otherwise.
local PALETTE = {}
for i = 0, 255 do
  local r, g, b = hue_to_rgb(i / 255.0)
  PALETTE[i] = { r, g, b }
end

return {
  identity = {
    vendor = "Example", model = "Effects",
    author = "Your Name", version = "1.0.0",
    description = "A pixmap plasma and a direct comet, to exercise custom RGB effects.",
  },
  type = "effect",

  effects = {
    {
      kind = "pixmap", id = "plasma", name = "Plasma",
      params = {
        {
          id = "speed", label = "Speed",
          kind = { kind = "range", min = 0.1, max = 3.0, step = 0.1 },
          default = 0.8,
        },
      },
    },
    {
      kind = "direct", id = "comet", name = "Comet",
      params = {
        {
          id = "color", label = "Color",
          kind = { kind = "color" },
          default = { r = 0, g = 160, b = 255 },
        },
        {
          id = "speed", label = "Speed",
          kind = { kind = "range", min = 0.05, max = 3.0, step = 0.05 },
          default = 0.3,
        },
        {
          id = "direction", label = "Direction",
          kind = { kind = "enum", options = { "forward", "backward" } },
          default = "forward",
        },
      },
    },
  },

  -- Pixmap: fill `buf` (a halod.buffer of canvas_w * canvas_h * 4 bytes, one
  -- linear-light RGBA quad per pixel) in place. No return value.
  --
  -- Three things keep 120000 pixels/frame of interpreted Lua fast enough:
  --   1. The x- and y-dependent sine terms are each precomputed once (400 +
  --      300 calls) instead of once per pixel (120000 calls).
  --   2. The color comes from the palette above instead of a per-pixel hue
  --      conversion.
  --   3. Each row is assembled as one Lua string (string.char/table.concat,
  --      pure Lua, no host call) and written with a single buf:set_bytes
  --      call, instead of 4 buf:set_u8 host calls per pixel (300 host calls
  --      per frame instead of 480000).
  render_plasma = function(buf, t, dt, params)
    local w, h = halod.canvas_w, halod.canvas_h
    local speed = params.speed or 0.8
    local sin, char, floor = math.sin, string.char, math.floor

    local col = {}
    for x = 0, w - 1 do
      col[x] = sin((x / w) * 10.0 + t * speed)
    end
    local row = {}
    for y = 0, h - 1 do
      row[y] = sin((y / h) * 8.0 - t * speed * 0.7)
    end

    local parts = {} -- reused scratch: one 4-byte string.char() per pixel in the row
    local concat = table.concat
    for y = 0, h - 1 do
      local ry = row[y]
      for x = 0, w - 1 do
        local v = 1.0 + 0.5 * col[x] + 0.5 * ry
        local c = PALETTE[floor((v * 0.5 % 1.0) * 255.0)]
        parts[x + 1] = char(c[1], c[2], c[3], 255)
      end
      buf:set_bytes(y * w * 4, concat(parts))
    end
  end,

  -- Direct: `leds` is an array of {p, p_ring, nx, ny} (p = fractional chain
  -- position). Return one {r, g, b} per LED, linear-light 0..1 — a comet
  -- head sweeping the chain with a short fading tail.
  led_colors_comet = function(leds, t, dt, params)
    local color = params.color or { r = 0, g = 160, b = 255 }
    local cr, cg, cb = color.r / 255.0, color.g / 255.0, color.b / 255.0
    local speed = params.speed or 0.3
    local dir = (params.direction == "backward") and -1.0 or 1.0
    local head = (t * speed * dir) % 1.0
    local out = {}
    for i, led in ipairs(leds) do
      local d = math.abs(led.p - head)
      d = math.min(d, 1.0 - d) -- wrap around the chain
      local bright = math.max(0.0, 1.0 - d * 8.0)
      out[i] = { r = cr * bright, g = cg * bright, b = cb * bright }
    end
    return out
  end,
}
