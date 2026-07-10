-- SPDX-License-Identifier: GPL-3.0-or-later
--
-- Built-in RGB effects plugin for HaloDaemon — ships compiled into the
-- daemon, so it's both the reference for the effect-plugin API and the
-- library of stock pixmap/direct effects (Plugins screen: enable/disable
-- like any other plugin).
--
-- An effect-only plugin (`type = "effect"`) declares no hardware match and
-- registers straight into the RGB engine's effect catalog:
--
--   * pixmap effects fill a shared 400x300 linear-RGBA buffer once per
--     frame; every zone using it then samples the buffer at its LED
--     positions.
--   * direct effects compute one color per LED directly from its chain
--     position, once per zone per frame.
--
-- Colors here are treated as plain 0..1 fractions of the sRGB byte value
-- (no gamma conversion) — the same convention the reference "comet" effect
-- established; the engine re-applies LED gamma on the way out. No
-- permissions are needed — effects are pure compute and never touch
-- hardware.

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

-- 256-entry rainbow palette, built once at load time so plasma's render loop
-- never computes a hue conversion per pixel — 120000 pixels/frame in
-- interpreted Lua adds up fast otherwise.
local PALETTE = {}
for i = 0, 255 do
  local r, g, b = hue_to_rgb(i / 255.0)
  PALETTE[i] = { r, g, b }
end

-- Scratch tables shared by every pixmap render below, hoisted so the worker
-- VM reuses them across frames instead of re-allocating (and GC'ing) a table
-- per frame. Each effect fully overwrites the slots it uses before reading
-- them, so no stale values leak between frames or between effects (only one
-- render_<id> callback ever runs in a given worker VM instance).
local COL, ROW, PARTS = {}, {}, {}

-- Eases `current` toward `target` with time constant `tau` seconds (`<= 0`
-- snaps instantly), so responsiveness is independent of the tick rate. Same
-- formula the native direct effects use.
local function ease_toward(current, target, tau, dt)
  local alpha
  if tau <= 1e-6 then
    alpha = 1.0
  else
    alpha = 1.0 - math.exp(-dt / tau)
  end
  return current + (target - current) * alpha
end

local function clamp01(v)
  return math.max(0.0, math.min(1.0, v))
end

-- Byte-space lerp between two {r,g,b} colors (0..255 channels), `tt` clamped
-- to 0..1. Returns 0..255 channel values.
local function lerp_color(a, b, tt)
  tt = clamp01(tt)
  return a.r + (b.r - a.r) * tt, a.g + (b.g - a.g) * tt, a.b + (b.b - a.b) * tt
end

local DEFAULT_STEPS = {
  { value = 40, color = { r = 0, g = 255, b = 0 } },
  { value = 60, color = { r = 255, g = 140, b = 0 } },
  { value = 80, color = { r = 255, g = 0, b = 0 } },
}

return {
  identity = {
    vendor = "HaloDaemon", model = "Halo Effects",
    author = "HaloDaemon", version = "1.0.0",
    description = "The stock library of pixmap and direct RGB effects, and the reference effect-plugin implementation.",
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
      kind = "pixmap", id = "rainbow", name = "Rainbow Wave",
      params = {
        {
          id = "speed", label = "Speed",
          kind = { kind = "range", min = 0.0, max = 2.0, step = 0.05 },
          default = 0.2,
        },
        {
          id = "scale", label = "Scale",
          kind = { kind = "range", min = 0.5, max = 5.0, step = 0.5 },
          default = 1.0,
        },
      },
    },
    {
      kind = "pixmap", id = "random_flash", name = "Random Flash",
      params = {
        {
          id = "cells", label = "Cells",
          kind = { kind = "range", min = 2.0, max = 8.0, step = 1.0 },
          default = 4.0,
        },
        {
          id = "interval", label = "Interval (s)",
          kind = { kind = "range", min = 0.2, max = 5.0, step = 0.1 },
          default = 1.0,
        },
        {
          id = "decay", label = "Decay (s)",
          kind = { kind = "range", min = 0.05, max = 3.0, step = 0.05 },
          default = 0.6,
        },
        {
          id = "random_color", label = "Random color per flash",
          kind = { kind = "boolean" },
          default = false,
        },
        {
          id = "color", label = "Color",
          kind = { kind = "color" },
          default = { r = 56, g = 189, b = 248 },
        },
      },
    },
    {
      kind = "pixmap", id = "audio_spectrum", name = "Audio Spectrum",
      params = {
        {
          id = "color_low", label = "Low Color",
          kind = { kind = "color" },
          default = { r = 0, g = 120, b = 255 },
        },
        {
          id = "color_high", label = "High Color",
          kind = { kind = "color" },
          default = { r = 255, g = 0, b = 120 },
        },
        {
          id = "fill", label = "Fill",
          kind = { kind = "enum", options = { "bars", "solid" } },
          default = "bars",
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
    {
      kind = "direct", id = "breathing", name = "Breathing",
      params = {
        {
          id = "color", label = "Color",
          kind = { kind = "color" },
          default = { r = 0, g = 128, b = 255 },
        },
        {
          id = "speed", label = "Speed",
          kind = { kind = "range", min = 0.1, max = 3.0, step = 0.1 },
          default = 0.5,
        },
      },
    },
    {
      kind = "direct", id = "audio_beat", name = "Audio Beat",
      params = {
        {
          id = "color", label = "Color",
          kind = { kind = "color" },
          default = { r = 255, g = 40, b = 40 },
        },
        {
          id = "decay", label = "Decay",
          kind = { kind = "range", min = 0.1, max = 2.0, step = 0.05 },
          default = 0.4,
        },
        {
          id = "sensitivity", label = "Sensitivity",
          kind = { kind = "range", min = 0.0, max = 1.0, step = 0.05 },
          default = 0.5,
        },
      },
    },
    {
      kind = "direct", id = "audio_level", name = "Audio Level",
      params = {
        {
          id = "color", label = "Color",
          kind = { kind = "color" },
          default = { r = 0, g = 200, b = 120 },
        },
        {
          id = "hue_shift", label = "Hue Shift",
          kind = { kind = "boolean" },
          default = false,
        },
        {
          id = "smoothing", label = "Smoothing",
          kind = { kind = "range", min = 0.0, max = 1.0, step = 0.05 },
          default = 0.3,
        },
        {
          id = "sensitivity", label = "Sensitivity",
          kind = { kind = "range", min = 0.1, max = 3.0, step = 0.1 },
          default = 1.0,
        },
      },
    },
    {
      kind = "direct", id = "sensor_gradient", name = "Sensor Gradient",
      params = {
        {
          id = "sensor", label = "Sensor",
          kind = { kind = "sensor" },
          default = "",
        },
        {
          id = "mode", label = "Mode",
          kind = { kind = "enum", options = { "gradient", "meter" } },
          default = "gradient",
        },
        {
          id = "color_a", label = "Color A",
          kind = { kind = "color" },
          default = { r = 0, g = 128, b = 255 },
        },
        {
          id = "color_b", label = "Color B",
          kind = { kind = "color" },
          default = { r = 255, g = 0, b = 0 },
        },
        {
          id = "min", label = "Min",
          kind = { kind = "number", min = -100000.0, max = 100000.0 },
          default = 20.0,
        },
        {
          id = "max", label = "Max",
          kind = { kind = "number", min = -100000.0, max = 100000.0 },
          default = 90.0,
        },
        {
          id = "smoothing", label = "Smoothing",
          kind = { kind = "range", min = 0.0, max = 1.0, step = 0.05 },
          default = 0.3,
        },
      },
    },
    {
      kind = "direct", id = "sensor_steps", name = "Sensor Steps",
      params = {
        {
          id = "sensor", label = "Sensor",
          kind = { kind = "sensor" },
          default = "",
        },
        {
          id = "steps", label = "Steps",
          kind = { kind = "steps" },
          default = DEFAULT_STEPS,
        },
        {
          id = "smoothing", label = "Smoothing",
          kind = { kind = "range", min = 0.0, max = 1.0, step = 0.05 },
          default = 0.3,
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

    local col, row, parts = COL, ROW, PARTS
    for x = 0, w - 1 do
      col[x] = sin((x / w) * 10.0 + t * speed)
    end
    for y = 0, h - 1 do
      row[y] = sin((y / h) * 8.0 - t * speed * 0.7)
    end

    local concat = table.concat
    for y = 0, h - 1 do
      local ry = row[y]
      for x = 0, w - 1 do
        local v = 1.0 + 0.5 * col[x] + 0.5 * ry
        -- frac < 1, so * 256 floors to exactly 0..255 (256 buckets, not 255)
        local c = PALETTE[floor((v * 0.5 % 1.0) * 256.0)]
        parts[x + 1] = char(c[1], c[2], c[3], 255)
      end
      buf:set_bytes(y * w * 4, concat(parts))
    end
  end,

  -- Pixmap: a horizontal hue sweep, one row built once and copied to every
  -- scanline (the color only varies with x).
  render_rainbow = function(buf, t, dt, params)
    local w, h = halod.canvas_w, halod.canvas_h
    local speed = params.speed or 0.2
    local scale = params.scale or 1.0
    local offset = t * speed
    local char, concat = string.char, table.concat

    local parts = PARTS
    for x = 0, w - 1 do
      local hue = (x / w) * scale + offset
      local r, g, b = hue_to_rgb(hue)
      parts[x + 1] = char(r, g, b, 255)
    end
    local row_bytes = concat(parts, "", 1, w)
    for y = 0, h - 1 do
      buf:set_bytes(y * w * 4, row_bytes)
    end
  end,

  -- Pixmap: one of `cells` equal columns lights up per `interval` seconds,
  -- decaying exponentially, deterministically picked (and never repeated
  -- back-to-back) from a hash of the epoch — a pure function of `t`, so it
  -- needs no persisted state across frames or worker restarts.
  render_random_flash = function(buf, t, dt, params)
    local w, h = halod.canvas_w, halod.canvas_h
    local cells = math.max(2, math.min(8, math.floor((params.cells or 4.0) + 0.5)))
    local interval = params.interval or 1.0
    local decay = params.decay or 0.6
    local random_color = params.random_color or false
    local color = params.color or { r = 56, g = 189, b = 248 }
    local char, floor, exp = string.char, math.floor, math.exp

    local function hash(seed)
      local s = math.sin(seed * 12.9898) * 43758.547
      return s - floor(s)
    end
    local function epoch_seed(epoch)
      return epoch % 16777213
    end
    local function pick(seed, prev)
      if cells <= 1 then
        return 0
      end
      local idx = floor(hash(seed) * cells) % cells
      if prev ~= nil and idx == prev then
        return (idx + 1) % cells
      end
      return idx
    end
    local function pick_for_epoch(epoch)
      local prev = nil
      if epoch > 0 then
        prev = pick(epoch_seed(epoch - 1), nil)
      end
      return pick(epoch_seed(epoch), prev)
    end

    local epoch = floor(math.max(t, 0.0) / interval)
    local idx = pick_for_epoch(epoch)
    local lit_at = epoch * interval
    local brightness = exp(-math.max(t - lit_at, 0.0) / decay)
    local r, g, b = color.r, color.g, color.b
    if random_color then
      r, g, b = hue_to_rgb(hash(epoch_seed(epoch) * 7.0 + 3.1))
    end
    local lr = floor(r * brightness + 0.5)
    local lg = floor(g * brightness + 0.5)
    local lb = floor(b * brightness + 0.5)

    local black_row = string.rep(char(0, 0, 0, 255), w)
    for y = 0, h - 1 do
      buf:set_bytes(y * w * 4, black_row)
    end
    local x0 = math.floor(idx * w / cells)
    local x1 = math.floor((idx + 1) * w / cells)
    if x1 > x0 then
      local lit_row = string.rep(char(lr, lg, lb, 255), x1 - x0)
      for y = 0, h - 1 do
        buf:set_bytes(y * w * 4 + x0 * 4, lit_row)
      end
    end
  end,

  -- Pixmap: `halod.audio().bands` (64 values, 0..1) as a bottom-anchored
  -- bar (or solid-fill) chart, each band lerped between color_low/high.
  render_audio_spectrum = function(buf, t, dt, params)
    local w, h = halod.canvas_w, halod.canvas_h
    local color_low = params.color_low or { r = 0, g = 120, b = 255 }
    local color_high = params.color_high or { r = 255, g = 0, b = 120 }
    local bars = params.fill ~= "solid"
    local audio = halod.audio()
    local bands = audio.bands
    local n = #bands
    local char, concat, floor = string.char, table.concat, math.floor
    local black = char(0, 0, 0, 255)

    local starts = {}
    for i = 1, n do
      local x0 = floor((i - 1) * w / n)
      local x1 = floor(i * w / n)
      if x1 < x0 then
        x1 = x0
      end
      local x_end = x1
      if bars and (x1 - x0) >= 2 then
        x_end = x1 - 1
      end
      local amp = clamp01(bands[i])
      local bar_h = floor(amp * h + 0.5)
      local tt = (i - 1) / math.max(n - 1, 1)
      local r, g, b = lerp_color(color_low, color_high, tt)
      starts[i] = {
        x0 = x0,
        x_end = x_end,
        y_start = h - bar_h,
        bytes = char(floor(r + 0.5), floor(g + 0.5), floor(b + 0.5), 255),
      }
    end

    local parts = PARTS
    for y = 0, h - 1 do
      for i = 1, n do
        local s = starts[i]
        local cb
        if y >= s.y_start then
          cb = s.bytes
        else
          cb = black
        end
        for x = s.x0, s.x_end - 1 do
          parts[x + 1] = cb
        end
      end
      buf:set_bytes(y * w * 4, concat(parts, "", 1, w))
    end
  end,

  -- Direct: `leds` is an array of {p, p_ring, nx, ny} (p = fractional chain
  -- position). Return one {r, g, b} per LED, 0..1 — a comet head sweeping
  -- the chain with a short fading tail.
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

  -- Direct: brightness pulses as sin(t*speed*pi)^2 — a pure function of `t`,
  -- so it's safe to call once per zone per tick without persisted state.
  led_colors_breathing = function(leds, t, dt, params)
    local color = params.color or { r = 0, g = 128, b = 255 }
    local speed = params.speed or 0.5
    local phase = math.sin(t * speed * math.pi)
    local bright = phase * phase
    local cr, cg, cb = color.r / 255.0 * bright, color.g / 255.0 * bright, color.b / 255.0 * bright
    local out = {}
    for i in ipairs(leds) do
      out[i] = { r = cr, g = cg, b = cb }
    end
    return out
  end,

  -- Direct: flashes on a loud onset (`halod.audio().flux` crossing a
  -- sensitivity-scaled threshold on a new DSP frame) and decays otherwise.
  -- `leds` is called once per zone per tick, all sharing the same `t`; the
  -- `last_t` guard below makes the state update idempotent across those
  -- repeat calls instead of double-decaying multi-zone devices.
  led_colors_audio_beat = (function()
    local state = { pulse = 0.0, last_seq = 0, last_t = nil }
    return function(leds, t, dt, params)
      local decay = params.decay or 0.4
      local sensitivity = params.sensitivity or 0.5
      if state.last_t ~= t then
        local audio = halod.audio()
        local threshold = 0.6 - 0.5 * sensitivity
        if audio.seq ~= state.last_seq and audio.flux >= threshold then
          state.pulse = 1.0
        else
          state.pulse = state.pulse * math.exp(-dt / (decay / 3.0))
        end
        state.last_seq = audio.seq
        state.last_t = t
      end
      local color = params.color or { r = 255, g = 40, b = 40 }
      local cr = color.r / 255.0 * state.pulse
      local cg = color.g / 255.0 * state.pulse
      local cb = color.b / 255.0 * state.pulse
      local out = {}
      for i in ipairs(leds) do
        out[i] = { r = cr, g = cg, b = cb }
      end
      return out
    end
  end)(),

  -- Direct: brightness eases toward `halod.audio().level`. Same `last_t`
  -- idempotency guard as audio_beat above.
  led_colors_audio_level = (function()
    local state = { display_level = 0.0, last_t = nil }
    local MAX_TAU = 0.5
    return function(leds, t, dt, params)
      local smoothing = params.smoothing or 0.3
      local sensitivity = params.sensitivity or 1.0
      if state.last_t ~= t then
        local audio = halod.audio()
        local target = clamp01(audio.level * sensitivity)
        local tau = clamp01(smoothing) * MAX_TAU
        state.display_level = ease_toward(state.display_level, target, tau, dt)
        state.last_t = t
      end
      local bright = state.display_level
      local r, g, b
      if params.hue_shift then
        local hue = 0.66 - 0.66 * bright
        r, g, b = hue_to_rgb(hue)
      else
        local color = params.color or { r = 0, g = 200, b = 120 }
        r, g, b = color.r, color.g, color.b
      end
      local out = {}
      local cr, cg, cb = r / 255.0 * bright, g / 255.0 * bright, b / 255.0 * bright
      for i in ipairs(leds) do
        out[i] = { r = cr, g = cg, b = cb }
      end
      return out
    end
  end)(),

  -- Direct: colors a zone (`mode = "gradient"`) or fills it up to the
  -- reading (`mode = "meter"`) along a two-stop gradient, normalized against
  -- [min, max]. `sensor` is the 5th callback arg: the live reading for the
  -- effect's declared `sensor` param, or nil while unset/unavailable — the
  -- effect fades to black rather than snapping when it disappears.
  led_colors_sensor_gradient = (function()
    local state = { display_level = 0.0, presence = 0.0, last_t = nil }
    local MAX_TAU = 5.0
    return function(leds, t, dt, params, sensor)
      local min = params.min or 20.0
      local max = params.max or 90.0
      local smoothing = params.smoothing or 0.3
      if state.last_t ~= t then
        local target_level, target_presence
        if sensor ~= nil then
          local range = max - min
          if math.abs(range) > 1e-6 then
            target_level = clamp01((sensor - min) / range)
          else
            target_level = 0.0
          end
          target_presence = 1.0
        else
          target_level, target_presence = state.display_level, 0.0
        end
        local tau = clamp01(smoothing) * MAX_TAU
        state.display_level = ease_toward(state.display_level, target_level, tau, dt)
        state.presence = ease_toward(state.presence, target_presence, tau, dt)
        state.last_t = t
      end

      local mode = params.mode or "gradient"
      local color_a = params.color_a or { r = 0, g = 128, b = 255 }
      local color_b = params.color_b or { r = 255, g = 0, b = 0 }
      local level, presence = state.display_level, state.presence
      local out = {}
      for i, led in ipairs(leds) do
        local r, g, b
        if mode == "meter" and led.p > level then
          r, g, b = 0.0, 0.0, 0.0
        elseif mode == "meter" then
          r, g, b = lerp_color(color_a, color_b, led.p)
        else
          r, g, b = lerp_color(color_a, color_b, level)
        end
        out[i] = { r = r / 255.0 * presence, g = g / 255.0 * presence, b = b / 255.0 * presence }
      end
      return out
    end
  end)(),

  -- Direct: snaps to the color of the highest step whose threshold the
  -- smoothed sensor reading has reached. Same `sensor` arg and fade-to-black
  -- semantics as sensor_gradient above.
  led_colors_sensor_steps = (function()
    local state = { display_value = nil, presence = 0.0, last_t = nil }
    local MAX_TAU = 5.0
    return function(leds, t, dt, params, sensor)
      local smoothing = params.smoothing or 0.3
      if state.last_t ~= t then
        local tau = clamp01(smoothing) * MAX_TAU
        if sensor ~= nil then
          if state.display_value == nil then
            state.display_value = sensor
          else
            state.display_value = ease_toward(state.display_value, sensor, tau, dt)
          end
        end
        local target_presence = (sensor ~= nil) and 1.0 or 0.0
        state.presence = ease_toward(state.presence, target_presence, tau, dt)
        state.last_t = t
      end

      local steps = params.steps or DEFAULT_STEPS
      local sorted = {}
      for i, s in ipairs(steps) do
        sorted[i] = s
      end
      table.sort(sorted, function(a, b) return a.value < b.value end)

      local color = { r = 0, g = 0, b = 0 }
      if state.display_value ~= nil and #sorted > 0 then
        local v = state.display_value
        local chosen = sorted[1]
        for i = 1, #sorted do
          if v >= sorted[i].value then
            chosen = sorted[i]
          end
        end
        color = chosen.color
      end
      local presence = state.presence
      local cr = color.r / 255.0 * presence
      local cg = color.g / 255.0 * presence
      local cb = color.b / 255.0 * presence
      local out = {}
      for i in ipairs(leds) do
        out[i] = { r = cr, g = cg, b = cb }
      end
      return out
    end
  end)(),
}
