-- SPDX-License-Identifier: GPL-3.0-or-later
-- SPDX-FileCopyrightText: tomasf <https://github.com/tomasf/evnia>
--
-- Philips Evnia 49M2C8900, ported from the native Rust driver to one built-in
-- Lua plugin. The monitor exposes two independent USB devices: a DDC/CI control
-- interface (via the USB hub chip, 2109:8884) for picture/OSD settings, and an
-- ENE KB7730 RGB controller (0cf2:b201) for the rear Ambiglow LEDs. This plugin
-- matches the DDC chip and opens the Ambiglow chip as a bundled secondary
-- `usb_control` endpoint, presenting both as a single device.
--
-- The 44-LED Ambiglow control path (capture-block enable, 0xE100 frame buffer,
-- baseline-region restore) is adapted from tomasf/evnia (MIT). DDC/CI details:
-- docs/protocols/ddc-ci.md; Ambiglow: docs/protocols/philips-ambiglow.md.

-- ── DDC/CI USB control-transfer parameters ───────────────────────────────────
local DDC_BMREQ_OUT = 0x40 -- vendor | host-to-device | device recipient
local DDC_BMREQ_IN  = 0xC0 -- vendor | device-to-host  | device recipient
local DDC_BREQ_WRITE = 0xB2
local DDC_BREQ_READ  = 0xA3
local DDC_READ_W_INDEX = 0x006F
local WRITE_GAP_MS = 50   -- minimum gap between DDC writes (MCCS §4.5)
local READ_DELAY_MS = 150 -- time the monitor needs to assemble a reply

-- ── Ambiglow (ENE) control-transfer parameters ───────────────────────────────
local AMBI_BMREQ_OUT = 0x40
local AMBI_BREQ = 0x80
local LED_COUNT = 44
local FRAME_ADDR = 0xE100
local CONTROL_BLOCKS = { 0xE020, 0xE030 }
local BASELINE_ADDR = 0xE020
local FRAME_SETTLE_MS = 10 -- let a frame settle before a baseline restore

-- 16-byte block that hands direct frame control to the host.
local CAPTURE_BLOCK = string.char(
  0x01, 0x00, 0x02, 0x04, 0x00, 0x05, 0x00, 0x00,
  0x00, 0x02, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x01)

-- 64-byte region that returns control to the monitor's own Ambiglow firmware.
local BASELINE_REGION = string.char(
  0x00, 0x01, 0x02, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x02, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00,
  0x00, 0x00, 0x02, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x02, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00,
  0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00,
  0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00)

-- ── Small helpers ────────────────────────────────────────────────────────────
local byte = string.byte

local function clamp(v, lo, hi)
  if v < lo then return lo elseif v > hi then return hi else return v end
end

-- ── DDC/CI packet encode / decode ────────────────────────────────────────────
-- Each builder takes the payload bytes and appends the XOR-fold checksum.
local function pack_with_xor(bytes)
  local x = 0
  for i = 1, #bytes do x = x ~ bytes[i] end
  bytes[#bytes + 1] = x
  return string.char(table.unpack(bytes))
end

local function build_write(vcp, value)
  return pack_with_xor({ 0x6e, 0x51, 0x84, 0x03, vcp, 0x00, value })
end

local function build_extended_set(sub, value)
  return pack_with_xor({ 0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, sub, 0x00, value })
end

local function build_get_standard(vcp)
  return pack_with_xor({ 0x6e, 0x51, 0x82, 0x01, vcp })
end

local function build_get_extended(sub)
  return pack_with_xor({ 0x6e, 0x51, 0x84, 0x01, 0xe2, 0xa0, sub })
end

local function build_get_info(a0, a1, a2, a3)
  return pack_with_xor({ 0x6e, 0x51, 0x86, 0x01, 0xfe, a0, a1, a2, a3 })
end

-- Standard MCCS get-VCP reply: 6e 88 02 00 vcp type maxH maxL curH curL xor.
-- Returns the current value, or nil on any malformed/errored reply (the caller
-- skips it, exactly as the native driver logged-and-skipped a failed read).
local function parse_get_reply(s)
  if #s < 12 or byte(s, 1) ~= 0x6e or byte(s, 3) ~= 0x02 or byte(s, 4) ~= 0x00 then
    return nil
  end
  local x = 0
  for i = 1, 10 do x = x ~ byte(s, i) end
  if byte(s, 11) ~= (0x50 ~ x) then return nil end
  return (byte(s, 9) << 8) | byte(s, 10)
end

-- Info-string reply: standard `02 fe <addr>` envelope or raw asset-EEPROM ASCII.
local function parse_info_reply(s)
  if #s < 4 or byte(s, 1) ~= 0x6e then return nil end
  local n = byte(s, 2) & 0x7f
  if #s < 2 + n + 1 then return nil end
  local x = 0
  for i = 1, 2 + n do x = x ~ byte(s, i) end
  if byte(s, 2 + n + 1) ~= (0x50 ~ x) then return nil end
  local from = 3
  if n >= 3 and byte(s, 3) == 0x02 and byte(s, 4) == 0xfe then from = 6 end
  local payload = string.sub(s, from, 2 + n)
  local nul = string.find(payload, "\0", 1, true)
  if nul then payload = string.sub(payload, 1, nul - 1) end
  return (payload:gsub("^%s+", ""):gsub("%s+$", ""))
end

-- ── VCP value tables (code, id/label) ────────────────────────────────────────
local INPUT_PORTS = {
  { code = 0x0F, label = "DisplayPort 1" },
  { code = 0x10, label = "DisplayPort 2" },
  { code = 0x11, label = "HDMI 1" },
  { code = 0x12, label = "HDMI 2" },
}

local OSD_LANGUAGES = {
  { code = 0x01, label = "Chinese (Traditional)" }, { code = 0x02, label = "English" },
  { code = 0x03, label = "French" }, { code = 0x04, label = "German" },
  { code = 0x05, label = "Italian" }, { code = 0x06, label = "Japanese" },
  { code = 0x07, label = "Korean" }, { code = 0x08, label = "Portuguese" },
  { code = 0x09, label = "Russian" }, { code = 0x0A, label = "Spanish" },
  { code = 0x0B, label = "Swedish" }, { code = 0x0C, label = "Turkish" },
  { code = 0x0D, label = "Chinese (Simplified)" }, { code = 0x0E, label = "Brazilian Portuguese" },
  { code = 0x12, label = "Czech" }, { code = 0x14, label = "Dutch" },
  { code = 0x16, label = "Finnish" }, { code = 0x17, label = "Greek" },
  { code = 0x1A, label = "Hungarian" }, { code = 0x1E, label = "Polish" },
  { code = 0x24, label = "Ukrainian" },
}

-- Unified SmartImage selector (SDR + HDR presets share VCP 0xDC).
local SMART_IMAGE_MODES = {
  { code = 0x00, label = "Standard" }, { code = 0x01, label = "FPS" },
  { code = 0x03, label = "Movie" }, { code = 0x04, label = "Game 1" },
  { code = 0x05, label = "Game 2" }, { code = 0x06, label = "Racing" },
  { code = 0x07, label = "RTS" }, { code = 0x08, label = "Economy" },
  { code = 0x0B, label = "LowBlue Mode" }, { code = 0x0E, label = "EasyRead" },
  { code = 0x11, label = "Console Mode" }, { code = 0x21, label = "HDR Game" },
  { code = 0x22, label = "HDR Movie" }, { code = 0x23, label = "HDR Vivid" },
  { code = 0x30, label = "HDR True Black" }, { code = 0x24, label = "HDR Personal" },
  { code = 0x20, label = "HDR Off" },
}

-- Indices (0-based) 11..15 mean "HDR is being applied"; HDR Off (16) does not.
local function smart_image_is_hdr_active(idx)
  return idx >= 11 and idx <= 15
end

local COLOR_TEMPERATURES = {
  { code = 0x02, label = "Native" }, { code = 0x04, label = "5000K" },
  { code = 0x05, label = "6500K" }, { code = 0x06, label = "7500K" },
  { code = 0x07, label = "8200K" }, { code = 0x08, label = "9300K" },
  { code = 0x0A, label = "11500K" }, { code = 0x0D, label = "Preset" },
}

local GAMMA_VALUES = {
  { code = 0x50, label = "1.0" }, { code = 0x64, label = "2.0" },
  { code = 0x78, label = "2.2" }, { code = 0x8C, label = "2.4" },
  { code = 0xA0, label = "2.6" },
}

local PIXEL_ORBITING = {
  { code = 0x00, label = "Off" }, { code = 0x02, label = "Slow" },
  { code = 0x03, label = "Normal" }, { code = 0x04, label = "Fast" },
}

local SCREEN_SAVER = {
  { code = 0x00, label = "Off" }, { code = 0x02, label = "Slow" }, { code = 0x03, label = "Fast" },
}

local POWER_MODES = { { code = 0x01, label = "On" }, { code = 0x04, label = "Standby" } }

local function options_from(tbl)
  local opts = {}
  for i, e in ipairs(tbl) do
    opts[i] = { id = tostring(i - 1), label = e.label }
  end
  return opts
end

-- Selected 0-based index -> raw VCP code.
local function code_at(tbl, selected)
  return tbl[math.min(selected, #tbl - 1) + 1].code
end

-- Raw VCP value -> selected 0-based index (low byte carries the code).
local function index_of_code(tbl, raw)
  local code = raw & 0xFF
  for i, e in ipairs(tbl) do
    if e.code == code then return i - 1 end
  end
  return 0
end

-- ── Ambiglow LED geometry (front-facing, x left→right, y top→bottom) ──────────
local function ambiglow_positions()
  local leds = {}
  local function push(id, x, y) leds[#leds + 1] = { id = id, x = x, y = y } end
  for i = 0, 3 do push(i, 0.98, 1.0 - i / 3.0) end            -- right edge, bottom→top
  for j = 0, 7 do push(4 + j, 0.90 - 0.38 * j / 7.0, 0.0) end -- top, right of center
  for j = 0, 7 do push(12 + j, 0.48 - 0.38 * j / 7.0, 0.0) end -- top, left of center
  for i = 0, 3 do push(20 + i, 0.02, i / 3.0) end             -- left edge, top→bottom
  for k = 0, 10 do push(24 + k, 0.5, 0.05 + 0.40 * k / 10.0) end -- upper center column
  for k = 0, 8 do push(35 + k, 0.5, 0.55 + 0.40 * k / 8.0) end   -- lower center column
  return leds
end

-- ── Transport helpers ────────────────────────────────────────────────────────
local function ddc_write(dev, payload)
  halod.sleep_ms(WRITE_GAP_MS)
  dev.transport:control_write("", DDC_BMREQ_OUT, DDC_BREQ_WRITE, 0x00, 0x00, payload)
end

local function ddc_read(dev)
  halod.sleep_ms(READ_DELAY_MS)
  return dev.transport:control_read("", DDC_BMREQ_IN, DDC_BREQ_READ, 0x00, DDC_READ_W_INDEX, 32)
end

local function ddc_get_standard(dev, vcp)
  ddc_write(dev, build_get_standard(vcp))
  return parse_get_reply(ddc_read(dev))
end

local function ddc_get_extended(dev, sub)
  ddc_write(dev, build_get_extended(sub))
  return parse_get_reply(ddc_read(dev))
end

local function ddc_get_info(dev, a0, a1, a2, a3)
  ddc_write(dev, build_get_info(a0, a1, a2, a3))
  return parse_info_reply(ddc_read(dev))
end

local function ambiglow_write(dev, address, data)
  dev.transport:control_write("ambiglow", AMBI_BMREQ_OUT, AMBI_BREQ, 0x00, address, data)
end

-- Arm direct frame control once (idempotent until a release).
local function ensure_capture(dev)
  if dev.captured then return end
  for _, addr in ipairs(CONTROL_BLOCKS) do
    ambiglow_write(dev, addr, CAPTURE_BLOCK)
  end
  dev.captured = true
end

-- Pack a colour list into the fixed LED_COUNT*3 RGB frame buffer (short list
-- leaves the tail black; extra colours are dropped).
local function build_frame(colors)
  local b = halod.buffer(LED_COUNT * 3)
  for i = 0, LED_COUNT - 1 do
    local c = colors[i + 1]
    if c then
      b:set_u8(i * 3, c.r)
      b:set_u8(i * 3 + 1, c.g)
      b:set_u8(i * 3 + 2, c.b)
    end
  end
  return b
end

local function write_colors(dev, colors)
  ensure_capture(dev)
  ambiglow_write(dev, FRAME_ADDR, build_frame(colors))
  dev.has_frame = true
end

-- Hand the LEDs back to monitor firmware (wait out any in-flight frame first).
local function release(dev)
  if dev.has_frame then halod.sleep_ms(FRAME_SETTLE_MS) end
  ambiglow_write(dev, BASELINE_ADDR, BASELINE_REGION)
  dev.captured = false
end

-- ── Manifest ─────────────────────────────────────────────────────────────────
local CAT_PICTURE, CAT_COLOR, CAT_AUDIO = "picture", "color", "audio"
local CAT_OSD, CAT_SETUP, CAT_GAMING = "osd", "setup", "gaming"
local CAT_SYSTEM_USB, CAT_SMART_IMAGE = "system_usb", "smart_image"

return {
  match = {
    transport = "usb_control", vid = 0x2109, pid = 0x8884,
    name = "Philips Evnia 49", device_type = "monitor",
  },
  identity = {
    vendor = "Philips", model = "49M2C8900", id = "philips_evnia",
    author = "HaloDaemon", version = "1.0.0",
    description = "Philips Evnia 49M2C8900 — DDC/CI picture/OSD controls + Ambiglow RGB.",
  },
  transports = {
    usb_control = {
      interface = 0,
      endpoints = { { id = "ambiglow", vid = 0x0CF2, pid = 0xB201, interface = 0 } },
    },
  },

  rgb = {
    zones = {
      { id = "ambiglow", name = "Ambiglow", topology = { type = "grid" }, leds = ambiglow_positions() },
    },
    native_effects = { { id = "monitor", name = "Monitor (firmware control)", params = {} } },
  },

  range = {
    ranges = {
      { key = "brightness", label = "Brightness", min = 0, max = 100, default = 50, category = CAT_PICTURE },
      { key = "contrast", label = "Contrast", min = 0, max = 100, default = 50, category = CAT_PICTURE },
      { key = "volume", label = "Volume", min = 0, max = 100, default = 0, category = CAT_AUDIO },
      { key = "sharpness", label = "Sharpness", min = 0, max = 100, default = 50, category = CAT_PICTURE },
      { key = "light_enhancement", label = "Light Enhancement", min = 0, max = 3, default = 0, category = CAT_PICTURE },
      { key = "color_enhancement", label = "Color Enhancement", min = 0, max = 3, default = 0, category = CAT_PICTURE },
      { key = "osd_h_position", label = "OSD Horizontal Position", min = 0, max = 100, default = 50, category = CAT_OSD },
      { key = "osd_v_position", label = "OSD Vertical Position", min = 0, max = 100, default = 50, category = CAT_OSD },
      { key = "power_led", label = "Power LED Brightness", min = 0, max = 4, default = 2,
        start_label = "Off", end_label = "Max", category = CAT_SETUP },
      { key = "gain_red", label = "Red Gain", min = 0, max = 100, default = 100, category = CAT_COLOR },
      { key = "gain_green", label = "Green Gain", min = 0, max = 100, default = 100, category = CAT_COLOR },
      { key = "gain_blue", label = "Blue Gain", min = 0, max = 100, default = 100, category = CAT_COLOR },
    },
  },

  choice = {
    choices = {
      { key = "input_source", label = "Input Source", options = options_from(INPUT_PORTS), default = 0 },
      { key = "smart_image", label = "SmartImage", options = options_from(SMART_IMAGE_MODES),
        default = 0, category = CAT_SMART_IMAGE },
      { key = "color_temperature", label = "Color Temperature", options = options_from(COLOR_TEMPERATURES),
        default = 0, category = CAT_PICTURE },
      { key = "gamma", label = "Gamma", options = options_from(GAMMA_VALUES), default = 2, category = CAT_PICTURE },
      { key = "pixel_orbiting", label = "Pixel Orbiting", options = options_from(PIXEL_ORBITING),
        default = 0, category = CAT_SETUP },
      { key = "screen_saver", label = "Screen Saver", options = options_from(SCREEN_SAVER),
        default = 0, category = CAT_SETUP },
      { key = "crosshair", label = "Crosshair", default = 0, category = CAT_GAMING,
        options = { { id = "0", label = "Off" }, { id = "1", label = "On" }, { id = "2", label = "Smart" } } },
      { key = "smart_response", label = "SmartResponse", default = 0, category = CAT_GAMING,
        options = { { id = "0", label = "Off" }, { id = "1", label = "Fast" },
          { id = "2", label = "Faster" }, { id = "3", label = "Fastest" } } },
      { key = "osd_transparency", label = "OSD Transparency", default = 0, category = CAT_OSD,
        options = { { id = "0", label = "Off" }, { id = "1", label = "1" }, { id = "2", label = "2" },
          { id = "3", label = "3" }, { id = "4", label = "4" } } },
      { key = "osd_timeout", label = "OSD Timeout", default = 0, category = CAT_OSD,
        options = { { id = "0", label = "5 s" }, { id = "1", label = "10 s" }, { id = "2", label = "20 s" },
          { id = "3", label = "30 s" }, { id = "4", label = "60 s" } } },
      { key = "osd_language", label = "OSD Language", options = options_from(OSD_LANGUAGES),
        default = 1, category = CAT_OSD },
      { key = "usb_c_setting", label = "USB-C Setting", default = 0, category = CAT_SYSTEM_USB,
        options = { { id = "0", label = "High Resolution (USB 2.0)" },
          { id = "1", label = "High Data Speed (USB 3.2)" } } },
      { key = "kvm", label = "KVM", default = 0, category = CAT_SYSTEM_USB,
        options = { { id = "0", label = "Auto" }, { id = "1", label = "USB Up" }, { id = "2", label = "USB-C" } } },
      { key = "power_mode", label = "Power", options = options_from(POWER_MODES), default = 0, category = CAT_SETUP },
    },
  },

  boolean = {
    booleans = {
      { key = "adaptive_sync", label = "Adaptive Sync", category = CAT_GAMING },
      { key = "low_input_lag", label = "Low Input Lag", category = CAT_GAMING },
      { key = "audio_mute", label = "Mute Audio", category = CAT_AUDIO },
      { key = "resolution_notice", label = "Resolution Notice", category = CAT_OSD },
      { key = "usb_standby", label = "USB Standby Mode", category = CAT_SYSTEM_USB },
      { key = "smart_power", label = "Smart Power", category = CAT_SETUP },
      { key = "cec", label = "CEC", category = CAT_SETUP },
      { key = "auto_warning", label = "Auto Warning", category = CAT_SETUP },
      { key = "srgb", label = "sRGB", category = CAT_PICTURE },
      { key = "hdr_active", label = "HDR Active", category = CAT_SMART_IMAGE, read_only = true },
    },
  },

  action = { actions = { { key = "pixel_refresh", label = "Pixel Refresh", category = CAT_SETUP } } },

  -- ── Callbacks ──────────────────────────────────────────────────────────────
  -- Both control endpoints are already open when this runs; read every VCP the
  -- UI surfaces and return the live values to seed the host range/choice caches.
  initialize = function(dev)
    dev.state = {}
    dev.captured = false
    dev.has_frame = false
    local s = dev.state
    local ranges, choices = {}, {}
    local v

    local model = ddc_get_info(dev, 0xE9, 0x0D, 0x00, 0x00)
    if model == "" then model = nil end

    v = ddc_get_standard(dev, 0x10); if v then ranges.brightness = clamp(v, 0, 100) end
    v = ddc_get_standard(dev, 0x12); if v then ranges.contrast = clamp(v, 0, 100) end
    v = ddc_get_standard(dev, 0x62); if v then ranges.volume = clamp(v, 0, 100) end
    v = ddc_get_standard(dev, 0x87); if v then ranges.sharpness = clamp(v, 0, 100) end
    v = ddc_get_standard(dev, 0xF2); if v then ranges.power_led = clamp(v, 0, 4) end
    v = ddc_get_standard(dev, 0x16); if v then ranges.gain_red = clamp(v, 0, 100) end
    v = ddc_get_standard(dev, 0x18); if v then ranges.gain_green = clamp(v, 0, 100) end
    v = ddc_get_standard(dev, 0x1A); if v then ranges.gain_blue = clamp(v, 0, 100) end
    v = ddc_get_extended(dev, 0x3D); if v then ranges.light_enhancement = clamp(v, 0, 3) end
    v = ddc_get_extended(dev, 0x3E); if v then ranges.color_enhancement = clamp(v, 0, 3) end
    v = ddc_get_extended(dev, 0x0E); if v then ranges.osd_h_position = clamp(v, 0, 100) end
    v = ddc_get_extended(dev, 0x0F); if v then ranges.osd_v_position = clamp(v, 0, 100) end

    v = ddc_get_standard(dev, 0x60); if v then choices.input_source = index_of_code(INPUT_PORTS, v) end
    v = ddc_get_standard(dev, 0xDC)
    if v then choices.smart_image = index_of_code(SMART_IMAGE_MODES, v); s.smart_image = choices.smart_image end
    v = ddc_get_standard(dev, 0x14); if v then choices.color_temperature = index_of_code(COLOR_TEMPERATURES, v) end
    v = ddc_get_standard(dev, 0x72); if v then choices.gamma = index_of_code(GAMMA_VALUES, v) end
    v = ddc_get_standard(dev, 0xCC); if v then choices.osd_language = index_of_code(OSD_LANGUAGES, v) end
    v = ddc_get_standard(dev, 0xEB); if v then choices.smart_response = math.min(v, 3) end
    v = ddc_get_standard(dev, 0xD6); if v then choices.power_mode = index_of_code(POWER_MODES, v) end
    v = ddc_get_extended(dev, 0x04); if v then choices.crosshair = math.min(v, 2) end
    v = ddc_get_extended(dev, 0x10); if v then choices.osd_transparency = math.min(v, 4) end
    v = ddc_get_extended(dev, 0x11); if v then choices.osd_timeout = math.min(v, 4) end
    v = ddc_get_extended(dev, 0x12); if v then choices.usb_c_setting = math.min(v, 1) end
    v = ddc_get_extended(dev, 0x15); if v then choices.kvm = math.min(v, 2) end
    v = ddc_get_extended(dev, 0x34); if v then choices.pixel_orbiting = index_of_code(PIXEL_ORBITING, v) end
    v = ddc_get_extended(dev, 0x35); if v then choices.screen_saver = index_of_code(SCREEN_SAVER, v) end

    v = ddc_get_extended(dev, 0x40); if v then s.adaptive_sync = v ~= 0 end
    v = ddc_get_extended(dev, 0x07); if v then s.low_input_lag = v ~= 0 end
    v = ddc_get_standard(dev, 0x8D); if v then s.audio_mute = v == 1 end
    v = ddc_get_standard(dev, 0xE9); if v then s.resolution_notice = v ~= 0 end
    v = ddc_get_extended(dev, 0x13); if v then s.usb_standby = v ~= 0 end
    v = ddc_get_extended(dev, 0x16); if v then s.smart_power = v ~= 0 end
    v = ddc_get_extended(dev, 0x17); if v then s.cec = v ~= 0 end
    v = ddc_get_extended(dev, 0x43); if v then s.auto_warning = v ~= 0 end
    v = ddc_get_extended(dev, 0x20); if v then s.srgb = v ~= 0 end

    log("Philips Evnia 49 initialized")
    return { ok = true, model = model, ranges = ranges, choices = choices }
  end,

  close = function(dev)
    if dev.captured then release(dev) end
  end,

  set_range = function(dev, key, value)
    if key == "brightness" then ddc_write(dev, build_write(0x10, clamp(value, 0, 100)))
    elseif key == "contrast" then ddc_write(dev, build_write(0x12, clamp(value, 0, 100)))
    elseif key == "volume" then ddc_write(dev, build_write(0x62, clamp(value, 0, 100)))
    elseif key == "sharpness" then ddc_write(dev, build_write(0x87, clamp(value, 0, 100)))
    elseif key == "power_led" then ddc_write(dev, build_write(0xF2, clamp(value, 0, 4)))
    elseif key == "gain_red" then ddc_write(dev, build_write(0x16, clamp(value, 0, 100)))
    elseif key == "gain_green" then ddc_write(dev, build_write(0x18, clamp(value, 0, 100)))
    elseif key == "gain_blue" then ddc_write(dev, build_write(0x1A, clamp(value, 0, 100)))
    elseif key == "light_enhancement" then ddc_write(dev, build_extended_set(0x3D, clamp(value, 0, 3)))
    elseif key == "color_enhancement" then ddc_write(dev, build_extended_set(0x3E, clamp(value, 0, 3)))
    elseif key == "osd_h_position" then ddc_write(dev, build_extended_set(0x0E, clamp(value, 0, 100)))
    elseif key == "osd_v_position" then ddc_write(dev, build_extended_set(0x0F, clamp(value, 0, 100)))
    else error("unknown range key: " .. key) end
  end,

  set_choice = function(dev, key, selected)
    if key == "input_source" then ddc_write(dev, build_write(0x60, code_at(INPUT_PORTS, selected)))
    elseif key == "osd_language" then ddc_write(dev, build_write(0xCC, code_at(OSD_LANGUAGES, selected)))
    elseif key == "smart_image" then
      ddc_write(dev, build_write(0xDC, code_at(SMART_IMAGE_MODES, selected)))
      dev.state = dev.state or {}
      dev.state.smart_image = selected -- keep hdr_active (a derived boolean) in sync
    elseif key == "color_temperature" then ddc_write(dev, build_write(0x14, code_at(COLOR_TEMPERATURES, selected)))
    elseif key == "gamma" then ddc_write(dev, build_write(0x72, code_at(GAMMA_VALUES, selected)))
    elseif key == "power_mode" then ddc_write(dev, build_write(0xD6, code_at(POWER_MODES, selected)))
    elseif key == "smart_response" then ddc_write(dev, build_write(0xEB, math.min(selected, 3)))
    elseif key == "pixel_orbiting" then ddc_write(dev, build_extended_set(0x34, code_at(PIXEL_ORBITING, selected)))
    elseif key == "screen_saver" then ddc_write(dev, build_extended_set(0x35, code_at(SCREEN_SAVER, selected)))
    elseif key == "crosshair" then ddc_write(dev, build_extended_set(0x04, math.min(selected, 2)))
    elseif key == "osd_transparency" then ddc_write(dev, build_extended_set(0x10, math.min(selected, 4)))
    elseif key == "osd_timeout" then ddc_write(dev, build_extended_set(0x11, math.min(selected, 4)))
    elseif key == "usb_c_setting" then ddc_write(dev, build_extended_set(0x12, math.min(selected, 1)))
    elseif key == "kvm" then ddc_write(dev, build_extended_set(0x15, math.min(selected, 2)))
    else error("unknown choice key: " .. key) end
  end,

  get_booleans = function(dev)
    local s = dev.state or {}
    return {
      { key = "adaptive_sync", value = s.adaptive_sync or false },
      { key = "low_input_lag", value = s.low_input_lag or false },
      { key = "audio_mute", value = s.audio_mute or false },
      { key = "resolution_notice", value = s.resolution_notice or false },
      { key = "usb_standby", value = s.usb_standby or false },
      { key = "smart_power", value = s.smart_power or false },
      { key = "cec", value = s.cec or false },
      { key = "auto_warning", value = s.auto_warning or false },
      { key = "srgb", value = s.srgb or false },
      { key = "hdr_active", value = smart_image_is_hdr_active(s.smart_image or 0), read_only = true },
    }
  end,

  set_boolean = function(dev, key, value)
    local s = dev.state or {}
    dev.state = s
    local on = value and 1 or 0
    if key == "adaptive_sync" then ddc_write(dev, build_extended_set(0x40, on)); s.adaptive_sync = value
    elseif key == "low_input_lag" then ddc_write(dev, build_extended_set(0x07, on)); s.low_input_lag = value
    elseif key == "audio_mute" then ddc_write(dev, build_write(0x8D, value and 0x01 or 0x02)); s.audio_mute = value
    elseif key == "resolution_notice" then ddc_write(dev, build_write(0xE9, value and 0x02 or 0x00)); s.resolution_notice = value
    elseif key == "usb_standby" then ddc_write(dev, build_extended_set(0x13, on)); s.usb_standby = value
    elseif key == "smart_power" then ddc_write(dev, build_extended_set(0x16, on)); s.smart_power = value
    elseif key == "cec" then ddc_write(dev, build_extended_set(0x17, on)); s.cec = value
    elseif key == "auto_warning" then ddc_write(dev, build_extended_set(0x43, on)); s.auto_warning = value
    elseif key == "srgb" then ddc_write(dev, build_extended_set(0x20, value and 0x02 or 0x00)); s.srgb = value
    else error("unknown boolean key: " .. key) end
  end,

  trigger_action = function(dev, key)
    if key == "pixel_refresh" then ddc_write(dev, build_extended_set(0x36, 0x01))
    else error("unknown action key: " .. key) end
  end,

  -- ── Ambiglow RGB ──────────────────────────────────────────────────────────
  apply = function(dev, state)
    if state.mode == "native_effect" and state.id == "monitor" then
      release(dev)
    elseif state.mode == "static" then
      local fill = {}
      for i = 1, LED_COUNT do fill[i] = state.color end
      write_colors(dev, fill)
    elseif state.mode == "per_led" then
      local map = (state.zones or {}).ambiglow or {}
      local fill = {}
      for i = 0, LED_COUNT - 1 do fill[i + 1] = map[tostring(i)] or { r = 0, g = 0, b = 0 } end
      write_colors(dev, fill)
    end
  end,

  write_frame = function(dev, zone_id, colors)
    if zone_id == "ambiglow" then write_colors(dev, colors) end
  end,
}
