-- SPDX-License-Identifier: GPL-3.0-or-later
--
-- Example device plugin for HaloDaemon.
--
-- A fictional HID cooler with an RGB ring, a controllable pump fan, and a
-- liquid-temperature sensor. It demonstrates every feature currently available
-- to plugins: RGB (descriptor + engine frames + mode apply), fan duty/rpm, a
-- background status poll, and the bounds-checked byte buffer.
--
-- Drop this in ~/.config/halod/plugins/ (edit the vid/pid to your hardware),
-- then open the Plugins screen and press Scan now. See docs/plugins.md.

local REPORT = 64

-- 24 LEDs evenly spaced around a ring, positions normalized to 0..1.
local function ring_leds(count)
  local leds = {}
  for i = 0, count - 1 do
    local a = (i / count) * 2 * math.pi
    leds[#leds + 1] = {
      id = i,
      x = 0.5 + 0.5 * math.cos(a),
      y = 0.5 + 0.5 * math.sin(a),
    }
  end
  return leds
end

return {
  match = { transport = "hid", vid = 0x1234, pid = 0x5678 },
  identity = { vendor = "Example", model = "Cooler X", name = "Example Cooler X" },
  transports = { hid = { report_size = REPORT, timeout_ms = 1000 } },

  rgb = {
    zones = {
      {
        id = "ring",
        name = "Pump Ring",
        topology = { type = "ring" },
        leds = ring_leds(24),
      },
    },
  },

  fan = { channel = 0 },
  sensor = {},
  poll = { interval_ms = 500 },

  -- Put the device into software/direct control mode.
  initialize = function(dev)
    dev.transport:write(halod.buffer(REPORT)) -- placeholder handshake
    log("example cooler initialized")
    return true
  end,

  -- Engine frame: one {r,g,b} per LED, in descriptor order.
  write_frame = function(dev, zone_id, colors)
    local pkt = halod.buffer(2 + 3 * #colors)
    pkt:set_u8(0, 0x22) -- opcode: direct LED frame
    pkt:set_u8(1, #colors)
    for i, c in ipairs(colors) do
      local base = 2 + (i - 1) * 3
      pkt:set_u8(base, c.r)
      pkt:set_u8(base + 1, c.g)
      pkt:set_u8(base + 2, c.b)
    end
    dev.transport:write(pkt)
  end,

  -- User-driven mode change (e.g. a solid colour picked in the UI).
  apply = function(dev, state)
    if state.mode == "static" then
      local c = state.color
      dev.write_frame(dev, "ring", { c }) -- fill from a single colour
    end
  end,

  -- Pump duty is 0..=255.
  set_duty = function(dev, duty)
    local pkt = halod.buffer(3)
    pkt:set_u8(0, 0x23) -- opcode: set pump duty
    pkt:set_u8(1, 0)    -- channel
    pkt:set_u8(2, duty)
    dev.transport:write(pkt)
  end,
  get_duty = function(dev)
    local s = dev.status or {}
    return s.pump_duty or 0
  end,
  get_rpm = function(dev)
    local s = dev.status or {}
    return s.pump_rpm
  end,

  -- Background poll parses one status report into dev.status.
  read_status = function(dev)
    local r = halod.buffer(dev.transport:read_nonblocking(REPORT))
    return {
      liquid_temp = r:get_u8(15),
      pump_rpm = r:get_u16_le(17),
      pump_duty = r:get_u8(19),
    }
  end,

  get_sensors = function(dev)
    local s = dev.status or {}
    return {
      {
        id = "liquid",
        name = "Liquid Temperature",
        value = s.liquid_temp or 0,
        unit = "celsius",
        sensor_type = "temperature",
      },
    }
  end,
}
