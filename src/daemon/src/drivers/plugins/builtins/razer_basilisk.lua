-- SPDX-License-Identifier: GPL-2.0-or-later
-- SPDX-FileCopyrightText: Terry Cain and OpenRazer contributors <https://github.com/openrazer/openrazer>
--
-- Razer Basilisk V3 mouse, ported from the native Rust driver to a built-in Lua
-- plugin: per-LED RGB (extended-matrix custom frame), DPI, and polling-rate.
--
-- Wire format: the fixed 90-byte `razer_report` sent as a HID *feature* report
-- with no report-id padding — hence `report_size = 0` (raw passthrough), so the
-- script builds the exact 91-byte buffer (leading report id + 90 payload). Byte
-- 89 is an XOR CRC over bytes 3..=88.

local TXID       = 0x1F -- Basilisk V3 transaction id
local LED_COUNT  = 11
local VARSTORE   = 0x01

-- Command (class, id, data_size) triples.
local CLASS_MISC       = 0x00
local ID_SET_POLLING   = 0x05
local CLASS_DPI        = 0x04
local ID_SET_DPI       = 0x05
local CLASS_EXT_MATRIX = 0x0F
local ID_EFFECT        = 0x02
local ID_CUSTOM_FRAME  = 0x03
local EFFECT_CUSTOM    = 0x08

-- Polling-rate wire codes, indexed by the choice option order below.
local POLL_CODES = { 0x01, 0x02, 0x08 } -- 1000 Hz, 500 Hz, 125 Hz

-- ── Report builder ───────────────────────────────────────────────────────────

-- Build the 91-byte wire buffer (report id + 90-byte razer_report). CRC at byte
-- 89 is the XOR of bytes 3..=88.
local function razer_report(class, id, data_size, args)
  local b = halod.buffer(91)
  b:set_u8(2, TXID)
  b:set_u8(6, data_size)
  b:set_u8(7, class)
  b:set_u8(8, id)
  for i = 1, math.min(#args, 80) do
    b:set_u8(8 + i, args[i])
  end
  local crc = 0
  for i = 3, 88 do
    crc = crc ~ b:get_u8(i)
  end
  b:set_u8(89, crc)
  return b
end

-- Extended-matrix custom-frame row args: [0,0,row,start_col,stop_col, RGB…].
local function custom_frame_args(row, start_col, colors)
  local n = #colors
  if n == 0 then return nil end
  local stop_col = start_col + n - 1
  if stop_col > 255 then error("Razer custom-frame run exceeds column 255") end
  local args = { 0x00, 0x00, row, start_col, stop_col }
  for i = 1, n do
    local c = colors[i] or { r = 0, g = 0, b = 0 }
    args[#args + 1] = c.r
    args[#args + 1] = c.g
    args[#args + 1] = c.b
  end
  return args
end

-- DPI set args (0x04 0x05): storage byte then big-endian X, Y, then two zero.
local function encode_dpi_xy(x, y)
  return { VARSTORE, (x >> 8) & 0xFF, x & 0xFF, (y >> 8) & 0xFF, y & 0xFF, 0x00, 0x00 }
end

local function write_leds(dev, colors)
  local args = custom_frame_args(0, 0, colors)
  if args then
    dev.transport:write(razer_report(CLASS_EXT_MATRIX, ID_CUSTOM_FRAME, 0x47, args))
  end
end

-- ── Plugin ───────────────────────────────────────────────────────────────────

return {
  match = {
    transport = "hid",
    vid = 0x1532,
    pids = { 0x0099 },
    interface = 3, -- the vendor control interface (OpenRazer wIndex 0x03)
  },
  identity = {
    vendor = "Razer", model = "Basilisk V3", id = "razer_basilisk",
    author = "HaloDaemon", version = "1.0.0",
    description = "Razer Basilisk V3 — RGB, DPI, and polling rate.",
  },
  -- Raw HID feature reports: build the exact 91-byte buffer ourselves.
  transports = { hid = { report_size = 0, feature_report = true, timeout_ms = 1000 } },

  rgb = { zones = {} }, -- reported dynamically by initialize()
  dpi = { min = 100, max = 26000, steps = { 800, 1600, 3200 } },
  choice = {
    choices = {
      {
        key = "poll_rate", label = "Polling Rate", category = "Mouse", display = "list",
        options = {
          { id = "1000 Hz", label = "1000 Hz" },
          { id = "500 Hz", label = "500 Hz" },
          { id = "125 Hz", label = "125 Hz" },
        },
        default = 0,
      },
    },
  },

  initialize = function(dev)
    -- Enable per-LED custom-frame mode (extended matrix effect 0x08).
    dev.transport:write(razer_report(CLASS_EXT_MATRIX, ID_EFFECT, 0x0C, { 0x00, 0x00, EFFECT_CUSTOM }))
    return {
      ok = true,
      zones = { { id = "mouse", name = "Lighting", topology = "linear", led_count = LED_COUNT } },
    }
  end,

  apply = function(dev, state)
    if state.mode == "static" then
      local colors = {}
      for i = 1, LED_COUNT do colors[i] = state.color end
      write_leds(dev, colors)
    elseif state.mode == "per_led" then
      local zone = state.zones and state.zones["mouse"]
      if zone then
        local colors = {}
        for i = 0, LED_COUNT - 1 do
          colors[i + 1] = zone[tostring(i)] or { r = 0, g = 0, b = 0 }
        end
        write_leds(dev, colors)
      end
    end
  end,

  -- Canvas-engine frame.
  write_frame = function(dev, _zone, colors)
    write_leds(dev, colors)
  end,

  set_dpi = function(dev, dpi)
    dev.transport:write(razer_report(CLASS_DPI, ID_SET_DPI, 0x07, encode_dpi_xy(dpi, dpi)))
  end,

  set_choice = function(dev, key, selected)
    if key ~= "poll_rate" then error("Razer: unknown choice key " .. tostring(key)) end
    local code = POLL_CODES[selected + 1]
    if code then
      dev.transport:write(razer_report(CLASS_MISC, ID_SET_POLLING, 0x01, { code }))
    end
  end,
}
