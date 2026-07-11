-- SPDX-License-Identifier: GPL-3.0-or-later
-- OpenRGB SDK network client — an integration plugin (no `match`, no hardware
-- bus). Connects to an OpenRGB server (default 127.0.0.1:6742) over TCP and
-- exposes each of its RGB controllers as a top-level HaloDaemon device.
--
-- Protocol: docs/protocols/openrgb.md, transcribed from the OpenRGB network
-- protocol description (https://github.com/Youda008/OpenRGB-cppSDK,
-- protocol_description.txt, "version 3" schema — the current stable one).
-- `DeviceDescription`/`ModeDescription`/`ZoneDescription` have no
-- version-conditional fields in that schema, so this client doesn't need to
-- branch on the negotiated protocol version to parse them correctly.
--
-- Scope: enumerates controllers and zones, and drives them via Direct/custom
-- mode (`SetCustomMode` + `UpdateZoneLEDs`). Mode enumeration/switching beyond
-- that, profiles, and plugin-to-plugin messages are out of scope.

local PKT_REQUEST_CONTROLLER_COUNT = 0
local PKT_REQUEST_CONTROLLER_DATA = 1
local PKT_REQUEST_PROTOCOL_VERSION = 40
local PKT_SET_CLIENT_NAME = 50
local PKT_RGBCONTROLLER_UPDATEZONELEDS = 1051
local PKT_RGBCONTROLLER_SETCUSTOMMODE = 1100

-- Highest protocol version this client speaks. Only used for the handshake;
-- it does not change how replies are parsed (see the module doc above).
local CLIENT_PROTOCOL_VERSION = 3

-- Per-controller-index: whether `SetCustomMode` has already been sent this
-- connection, so a fast effect loop doesn't re-send it (and risk a visible
-- reset) on every single frame.
local custom_mode_sent = {}

-- ── wire framing ──────────────────────────────────────────────────────────

local function send_packet(dev, device_idx, packet_id, payload)
  payload = payload or ""
  local header = string.pack("<c4I4I4I4", "ORGB", device_idx, packet_id, #payload)
  dev.transport:write(header .. payload)
end

-- Reads the next reply header, returning (device_idx, packet_id, payload_size).
local function recv_header(dev)
  local magic = dev.transport:read(4)
  if magic ~= "ORGB" then
    error("openrgb: bad magic in reply header: " .. tostring(magic))
  end
  return string.unpack("<I4I4I4", dev.transport:read(12))
end

-- Several client->server payloads repeat their own byte length as a leading
-- u32 (equal to the packet header's size field) — "yes, this value is really
-- there twice" per the protocol doc. `rest` is everything after that field.
local function with_data_size(rest)
  return string.pack("<I4", 4 + #rest) .. rest
end

-- ── binary readers over an already-received payload string (1-based pos) ──

local function read_u16(data, pos)
  return string.unpack("<I2", data, pos)
end

local function read_u32(data, pos)
  return string.unpack("<I4", data, pos)
end

-- Server strings are length-prefixed but not null-terminated.
local function read_str(data, pos)
  local len, p = read_u16(data, pos)
  if len == 0 then
    return "", p
  end
  return data:sub(p, p + len - 1), p + len
end

-- Skip one ModeDescription entry. Its 12 uint32 fields (value, flags,
-- speed_min/max, brightness_min/max, colors_min/max, speed, brightness,
-- direction, color_mode) are always present regardless of ModeFlags — only
-- their *meaning* depends on the flags, not their presence on the wire.
local function skip_mode(data, pos)
  local _, p = read_str(data, pos)
  p = p + 4 * 12
  local num_colors
  num_colors, p = read_u16(data, p)
  return p + num_colors * 4
end

-- Read one ZoneDescription entry. `id` is always the zone's 0-based ordinal
-- as a string — that's what `write_controller_frame` needs to address it
-- with `UpdateZoneLEDs` (which takes a zone *index*, not a name).
local function read_zone(data, pos, zero_based_index)
  local name, p = read_str(data, pos)
  local _type
  _type, p = read_u32(data, p) -- ZoneType: not needed to build an RgbZone
  p = p + 4 -- leds_min
  p = p + 4 -- leds_max
  local leds_count
  leds_count, p = read_u32(data, p)
  local matrix_len
  matrix_len, p = read_u16(data, p)
  if matrix_len > 0 then
    p = p + matrix_len -- byte length of the whole optional matrix block
  end
  local zone = {
    id = tostring(zero_based_index),
    name = (name ~= "" and name) or ("Zone " .. (zero_based_index + 1)),
    topology = "linear",
    led_count = leds_count,
  }
  return zone, p
end

local function pack_colors(colors)
  local parts = {}
  for i, c in ipairs(colors) do
    parts[i] = string.char(c.r, c.g, c.b, 0)
  end
  return table.concat(parts)
end

return {
  type = "integration",
  identity = {
    vendor = "OpenRGB",
    model = "SDK Client",
    name = "OpenRGB",
    author = "HaloDaemon",
    description = "Connects to an OpenRGB server and exposes its controllers as devices.",
  },
  permissions = { "network" },
  config = {
    fields = {
      { key = "host", label = "Server host", kind = "text", default = "127.0.0.1" },
      { key = "port", label = "Server port", kind = "number", default = "6742" },
    },
  },
  transports = {
    tcp = { host_key = "host", port_key = "port", timeout_ms = 5000 },
  },

  initialize = function(dev)
    send_packet(dev, 0, PKT_SET_CLIENT_NAME, "HaloDaemon\0")
    send_packet(dev, 0, PKT_REQUEST_PROTOCOL_VERSION, string.pack("<I4", CLIENT_PROTOCOL_VERSION))
    local _, _, size = recv_header(dev)
    if size > 0 then
      dev.transport:read(size) -- server's max version; unused (fixed reply layout)
    end
    return true
  end,

  enumerate_controllers = function(dev)
    send_packet(dev, 0, PKT_REQUEST_CONTROLLER_COUNT)
    local _, _, csize = recv_header(dev)
    local count = read_u32(dev.transport:read(csize), 1)

    local controllers = {}
    for index = 0, count - 1 do
      send_packet(dev, index, PKT_REQUEST_CONTROLLER_DATA, string.pack("<I4", CLIENT_PROTOCOL_VERSION))
      local _, _, dsize = recv_header(dev)
      local data = dev.transport:read(dsize)

      -- Leading duplicate data_size (u32), then DeviceDescription.
      local pos = 5
      pos = pos + 4 -- type
      local name
      name, pos = read_str(data, pos)
      local _description
      _description, pos = read_str(data, pos)
      local _version
      _version, pos = read_str(data, pos)
      local _serial
      _serial, pos = read_str(data, pos)
      local _location
      _location, pos = read_str(data, pos)

      local num_modes
      num_modes, pos = read_u16(data, pos)
      pos = pos + 4 -- active_mode
      for _ = 1, num_modes do
        pos = skip_mode(data, pos)
      end

      local num_zones
      num_zones, pos = read_u16(data, pos)
      local zones = {}
      for z = 1, num_zones do
        local zone
        zone, pos = read_zone(data, pos, z - 1)
        zones[#zones + 1] = zone
      end
      -- LEDs/colors sections follow but aren't needed for our descriptor; the
      -- whole message was already consumed via `dsize` above, so there's
      -- nothing left to skip.

      controllers[#controllers + 1] = {
        index = index,
        name = (name ~= "" and name) or ("Controller " .. index),
        zones = zones,
      }
    end
    return controllers
  end,

  write_controller_frame = function(dev, index, zone, colors)
    if not custom_mode_sent[index] then
      send_packet(dev, index, PKT_RGBCONTROLLER_SETCUSTOMMODE)
      custom_mode_sent[index] = true
    end
    local zone_idx = tonumber(zone) or 0
    local rest = string.pack("<I4", zone_idx) .. string.pack("<I2", #colors) .. pack_colors(colors)
    send_packet(dev, index, PKT_RGBCONTROLLER_UPDATEZONELEDS, with_data_size(rest))
  end,
}
