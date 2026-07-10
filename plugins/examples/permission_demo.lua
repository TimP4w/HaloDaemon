-- SPDX-License-Identifier: GPL-3.0-or-later
--
-- Example plugin demonstrating the permission system.
--
-- Declares `permissions = { "os" }`, so the daemon keeps it inert (loaded but
-- never matched against hardware) until you grant it in the GUI. Once
-- granted, `os.time()`/`os.clock()` become callable inside its Lua VM — every
-- other `os.*` function (file/process access) stays stripped regardless.
--
-- How to test:
--   1. Drop this file in ~/.config/halod/plugins/ and press "Scan now" (or
--      restart the daemon) so it's picked up.
--   2. Open the Plugins screen. The plugin is listed (no hardware needed to
--      see it) with an ungranted "os" permission chip and a pending notice.
--   3. Click "Grant permissions" — the chip turns green and a "Revoke"
--      button appears. This is persisted across restarts.
--   4. Revoking removes the grant again; the plugin goes back to inert next
--      time a matching device would otherwise be discovered.
--
-- The `match` below targets a placeholder vid/pid, so it never actually
-- attaches to a real device — this plugin exists purely to exercise the
-- consent flow. See example_device.lua for a full working device driver.

return {
  match = { transport = "hid", vid = 0xFFFE, pid = 0xFFFE },
  identity = {
    vendor = "Example", model = "Permission Demo",
    author = "Your Name", version = "1.0.0",
    description = "Exercises the plugin permission/consent flow. No real hardware required.",
  },

  permissions = { "os" },

  sensor = {},

  initialize = function(dev)
    log("permission_demo initialized")
    return true
  end,

  -- Only reachable if this plugin ever matches a real device — proves the
  -- granted "os" permission actually re-enables the read-only wall clock.
  get_sensors = function(dev)
    local now = os.time()
    return {
      { id = "clock", name = "Granted-at", value = now, unit = "", sensor_type = "other" },
    }
  end,
}
