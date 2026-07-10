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
      },
    },
  },

  -- Pixmap: fill `buf` (a halod.buffer of canvas_w * canvas_h * 4 bytes, one
  -- linear-light RGBA quad per pixel) in place. No return value.
  render_plasma = function(buf, t, dt, params)
    local w, h = halod.canvas_w, halod.canvas_h
    local speed = params.speed or 0.8
    for y = 0, h - 1 do
      local fy = y / h
      for x = 0, w - 1 do
        local fx = x / w
        local v = 0.5 + 0.5 * math.sin(fx * 10.0 + t * speed)
                       + 0.5 + 0.5 * math.sin(fy * 8.0 - t * speed * 0.7)
        local r, g, b = halod.hsv((v / 2.0) % 1.0, 1.0, 1.0)
        local i = (y * w + x) * 4
        buf:set_u8(i, r)
        buf:set_u8(i + 1, g)
        buf:set_u8(i + 2, b)
        buf:set_u8(i + 3, 255)
      end
    end
  end,

  -- Direct: `leds` is an array of {p, p_ring, nx, ny} (p = fractional chain
  -- position). Return one {r, g, b} per LED, linear-light 0..1 — a bare
  -- comet head sweeping the chain with a short fading tail.
  led_colors_comet = function(leds, t, dt, params)
    local color = params.color or { r = 0, g = 160, b = 255 }
    local cr, cg, cb = color.r / 255.0, color.g / 255.0, color.b / 255.0
    local head = t % 1.0
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
