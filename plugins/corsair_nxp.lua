-- SPDX-License-Identifier: GPL-2.0-or-later
-- SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project
--
-- Corsair NXP keyboard protocol — Lua plugin for HaloDaemon.
--
-- Port of daemon/src/drivers/vendors/corsair/protocols/corsair_nxp.rs
-- and daemon/src/drivers/vendors/corsair/devices/nxp_keyboard.rs.
--
-- Currently supports the K70 RGB MK.2 Low Profile (PID 0x1B55).
-- Adding another NXP keyboard is a new model entry in the MODELS table.

-- ── protocol constants ─────────────────────────────────────────────────────

local REPORT_SIZE = 65

local CMD_WRITE             = 0x07
local CMD_READ              = 0x0E
local CMD_STREAM            = 0x7F

local PROP_FIRMWARE         = 0x01
local PROP_SPECIAL_FUNCTION = 0x04
local PROP_LIGHTING_CONTROL = 0x05
local PROP_LAYOUT_SETUP     = 0x40
local PROP_SUBMIT_KEYBOARD  = 0x28

local LAYOUT_SETUP_SUB      = 0x1E
local LAYOUT_PRIMER_MODE    = 0x08
local LAYOUT_KEY_TRAILER    = 0xC0
local LAYOUT_KEYS_PER_PKT   = 30
local LAYOUT_PACKET_COUNT   = 4

local HARDWARE = 0x01
local SOFTWARE = 0x02

local CLASS_KEYBOARD = 0x03

local CH_RED   = 0x01
local CH_GREEN = 0x02
local CH_BLUE  = 0x03

local STREAM_CHUNK = 60

local CHANNELS = {
	{ ch = CH_RED,   extract = function(c) return c.r end },
	{ ch = CH_GREEN, extract = function(c) return c.g end },
	{ ch = CH_BLUE,  extract = function(c) return c.b end },
}

-- Key identifiers omitted from the layout-setup burst.
local SKIP_ANSI = {
	0x31, 0x3f, 0x41, 0x42, 0x51, 0x53, 0x55, 0x6f, 0x7e, 0x7f, 0x80, 0x81,
}
local SKIP_ISO_K70_MK2 = {
	0x3f, 0x41, 0x42, 0x50, 0x53, 0x55, 0x6f,
	0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f, 0x80, 0x81,
}

local LAYOUT_SKIP = {
	["ch"] = SKIP_ISO_K70_MK2,
	["it"] = SKIP_ISO_K70_MK2,
	["us"] = SKIP_ANSI,
}

local LAYOUT_NAMES = {
	["ch"] = "Swiss (ISO)",
	["it"] = "Italian (ISO)",
	["us"] = "US (ANSI)",
}

-- ── model table ─────────────────────────────────────────────────────────────

-- K70 MK.2 LED order → device key-id. 116 entries.
local K70_MK2_KEYS = {
	0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0C, 0x0D, 0x0E, 0x0F, 0x11, 0x12,
	0x14, 0x15, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x20, 0x21, 0x24, 0x25, 0x26,
	0x27, 0x28, 0x2A, 0x2B, 0x2C, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
	0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x42, 0x43, 0x44, 0x45, 0x48, 73, 74, 75, 76, 78,
	79, 80, 81, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 96, 97,
	98, 99, 100, 101, 102, 103, 104, 105, 108, 109, 110, 111, 112, 113, 115,
	116, 117, 120, 121, 122, 123, 124, 126, 127, 128, 129, 132, 133, 134, 135,
	136, 137, 139, 140, 141, 16, 114, 47, 59, 125,
}

-- Physical key grid (7 rows × 23 columns). Cells are LED indices into
-- K70_MK2_KEYS; -1 is empty.
local NA = -1
local K70_MK2_MATRIX = {
	{ NA, NA, NA, 115, 107,  8, NA, NA, NA, NA, NA, 113, 114, NA, NA, NA, NA, NA, NA,  16, NA, NA,  NA },
	{  0, NA, 10,  18,  28, 36, NA, 46, 55, 64, 74,  NA,  84, 93, 102,  6, 15, 24, 33,  26, 35, 44,  53 },
	{  1, 11, 19,  29,  37, 47, 56, 65, 75, 85, 94,  NA, 103,  7,  25, NA, 42, 51, 60,  62, 72, 82,  91 },
	{  2, NA, 12,  20,  30, 38, NA, 48, 57, 66, 76,  86,  95, 104, 70, 80, 34, 43, 52,   9, 17, 27, 100 },
	{  3, NA, 13,  21,  31, 39, NA, 49, 58, 67, 77,  87,  96, 105, 98, 112, NA, NA, NA, 45, 54, 63,  NA },
	{  4, 111, 22, 32,  40, 50, NA, 59, NA, 68, 78,  88,  97, 106, 61, NA, NA, 81, NA,  73, 83, 92, 109 },
	{  5, 14, 23,  NA,  NA, NA, NA, 41, NA, NA, NA,  NA,  69, 79,  89, 71, 90, 99, 108, 101, NA, 110, NA },
}

local MODELS = {
	[0x1B55] = {
		name = "K70 RGB MK.2 Low Profile",
		model = "K70 RGB MK.2 Low Profile",
		keys = K70_MK2_KEYS,
		matrix = K70_MK2_MATRIX,
		wire_len = 144,
	},
}

-- ── protocol helpers ───────────────────────────────────────────────────────

local function frame(payload)
	local buf = halod.buffer(REPORT_SIZE)
	for i, b in ipairs(payload) do
		buf:set_u8(i, b)  -- byte 0 stays 0x00 (report id); payload at [1..]
	end
	return buf
end

local function special_function(software)
	local mode = software and SOFTWARE or HARDWARE
	return frame({ CMD_WRITE, PROP_SPECIAL_FUNCTION, mode })
end

local function lighting_control(software, class)
	local mode = software and SOFTWARE or HARDWARE
	return frame({ CMD_WRITE, PROP_LIGHTING_CONTROL, mode, 0x00, class })
end

local function firmware_query()
	return frame({ CMD_READ, PROP_FIRMWARE })
end

local function stream_packet(nonce, data)
	local payload = { CMD_STREAM, nonce, #data, 0x00 }
	for _, b in ipairs(data) do
		payload[#payload + 1] = b
	end
	return frame(payload)
end

local function submit_keyboard_24(channel, packet_count, finish)
	return frame({ CMD_WRITE, PROP_SUBMIT_KEYBOARD, channel, packet_count, finish })
end

-- Encode a wire buffer (indexed by device key-id) into the NXP stream packets.
local function color_frame_packets(colors)
	local out = {}
	for _, chdef in ipairs(CHANNELS) do
		local plane = {}
		for _, c in ipairs(colors) do
			plane[#plane + 1] = chdef.extract(c)
		end
		-- chunk into STREAM_CHUNK (60) groups
		local chunks = {}
		for i = 1, #plane, STREAM_CHUNK do
			local chunk = {}
			for j = i, math.min(i + STREAM_CHUNK - 1, #plane) do
				chunk[#chunk + 1] = plane[j]
			end
			chunks[#chunks + 1] = chunk
		end
		for i, chunk in ipairs(chunks) do
			out[#out + 1] = stream_packet(i, chunk)
		end
		local finish = (chdef.ch == CH_BLUE) and 2 or 1
		out[#out + 1] = submit_keyboard_24(chdef.ch, #chunks, finish)
	end
	return out
end

-- Build the layout-setup burst (5 packets: primer + 4×30 key-ids).
local function layout_setup_packets(skip)
	local out = {}
	out[#out + 1] = frame({ CMD_WRITE, PROP_LIGHTING_CONTROL, LAYOUT_PRIMER_MODE, 0x00, 0x01 })

	-- Build skip set for O(1) lookup.
	local skip_set = {}
	for _, s in ipairs(skip) do
		skip_set[s] = true
	end

	local id = 0
	for _ = 1, LAYOUT_PACKET_COUNT do
		local payload = { CMD_WRITE, PROP_LAYOUT_SETUP, LAYOUT_SETUP_SUB, 0x00 }
		for _ = 1, LAYOUT_KEYS_PER_PKT do
			while skip_set[id] do
				id = id + 1
			end
			payload[#payload + 1] = id
			payload[#payload + 1] = LAYOUT_KEY_TRAILER
			id = id + 1
		end
		out[#out + 1] = frame(payload)
	end
	return out
end

-- ── model helpers ───────────────────────────────────────────────────────────

local function model_for(pid)
	return MODELS[pid]
end

-- Compute LED positions from the physical matrix.
local function led_positions(spec)
	local rows = #spec.matrix
	local cols = #spec.matrix[1]
	local nkeys = #spec.keys
	local positions = {}
	for i = 1, nkeys do
		positions[i] = { id = i - 1, x = 0.5, y = 0.5 }
	end
	for r = 1, rows do
		for c = 1, cols do
			local cell = spec.matrix[r][c]
			if cell >= 0 then
				local idx = cell + 1 -- Lua is 1-indexed
				if positions[idx] then
					positions[idx].x = (c - 1) / (cols - 1)
					positions[idx].y = (r - 1) / (rows - 1)
				end
			end
		end
	end
	return positions
end

-- ── per-device helpers (called with dev and its spec) ──────────────────────

-- Scatter LED-ordered colors into the wire buffer indexed by device key-id.
local function wire_buffer(spec, led_colors)
	local buf = {}
	for _ = 1, spec.wire_len do
		buf[#buf + 1] = { r = 0, g = 0, b = 0 }
	end
	for i, key_id in ipairs(spec.keys) do
		local color = led_colors[i]
		if color then
			buf[key_id + 1] = color -- key_id → 1-based index
		end
	end
	return buf
end

-- ── plugin table ────────────────────────────────────────────────────────────

-- The plugin stores per-device state keyed by the device's transport path.
-- In the plugin model each callback receives `dev`; we stash the model spec
-- and current layout in the dev table so they survive across calls.
-- Actually, `dev` is re-created per call, so we use a weak-keyed cache.
local device_state = setmetatable({}, { __mode = "k" })

local function ensure_state(dev)
	if not device_state[dev] then
		local spec = model_for(dev.match.pid)
		device_state[dev] = {
			spec = spec,
			layout = "ch",  -- default Swiss ISO
		}
	end
	return device_state[dev]
end

return {
	match = {
		transport = "hid",
		vid = 0x1B1C,
		pid = 0x1B55,
		interface = 1,
	},

	identity = {
		vendor = "Corsair",
		model = "K70 RGB MK.2 Low Profile",
		name = "Corsair K70 RGB MK.2 Low Profile",
		author = "HaloDaemon (ported from OpenRGB)",
		version = "1.0.0",
		description = "Corsair NXP keyboard protocol driver. Supports the K70 RGB MK.2 Low Profile.",
	},

	transports = { hid = { report_size = 0, timeout_ms = 1000 } },

	rgb = {
		zones = {
			{
				id = "keyboard",
				name = "Keyboard",
				topology = { type = "grid" },
				leds = led_positions(MODELS[0x1B55]),
			},
		},
	},

	choice = {
		choices = {
			{
				key = "layout",
				label = "Physical Layout",
				category = "Keyboard",
				display = "list",
				options = {
					{ id = "ch", label = "Swiss (ISO)" },
					{ id = "it", label = "Italian (ISO)" },
					{ id = "us", label = "US (ANSI)" },
				},
				default = 0,
			},
		},
	},

	-- ── callbacks ─────────────────────────────────────────────────────────

	initialize = function(dev)
		local st = ensure_state(dev)
		local spec = st.spec
		if not spec then
			log("corsair_nxp: unknown pid " .. string.format("0x%04x", dev.match.pid))
			return false
		end

		-- Enter software/direct control mode.
		dev.transport:write(firmware_query())
		dev.transport:write(special_function(true))
		dev.transport:write(lighting_control(true, CLASS_KEYBOARD))

		-- Send layout setup.
		local skip = LAYOUT_SKIP[st.layout] or SKIP_ISO_K70_MK2
		local pkts = layout_setup_packets(skip)
		dev.transport:write_many(pkts)

		log("corsair_nxp: " .. spec.name .. " initialized")
		return true
	end,

	close = function(dev)
		dev.transport:write(special_function(false))
	end,

	write_frame = function(dev, zone_id, colors)
		if zone_id ~= "keyboard" then return end
		local st = ensure_state(dev)
		local wbuf = wire_buffer(st.spec, colors)
		local pkts = color_frame_packets(wbuf)
		dev.transport:write_many(pkts)
	end,

	apply = function(dev, state)
		local st = ensure_state(dev)
		local spec = st.spec
		local led_count = #spec.keys

		if state.mode == "static" then
			local c = state.color
			local colors = {}
			for _ = 1, led_count do
				colors[#colors + 1] = { r = c.r, g = c.g, b = c.b }
			end
			local wbuf = wire_buffer(spec, colors)
			dev.transport:write_many(color_frame_packets(wbuf))
		elseif state.mode == "per_led" then
			-- Per-LED map keyed by LED index string.
			local colors = {}
			local black = { r = 0, g = 0, b = 0 }
			for i = 0, led_count - 1 do
				local key = tostring(i)
				local c = (state.zones and state.zones["keyboard"] and state.zones["keyboard"][key]) or black
				colors[#colors + 1] = { r = c.r, g = c.g, b = c.b }
			end
			local wbuf = wire_buffer(spec, colors)
			dev.transport:write_many(color_frame_packets(wbuf))
		end
		-- "engine" mode is handled by write_frame; "native_effect" / "direct_effect" are no-ops.
	end,

	set_choice = function(dev, key, selected)
		if key ~= "layout" then return end
		local st = ensure_state(dev)
		local layouts = { "ch", "it", "us" }
		local new_layout = layouts[selected + 1] or "ch"
		st.layout = new_layout

		local skip = LAYOUT_SKIP[new_layout] or SKIP_ISO_K70_MK2
		dev.transport:write_many(layout_setup_packets(skip))
		log("corsair_nxp: layout set to " .. (LAYOUT_NAMES[new_layout] or new_layout))
	end,
}
