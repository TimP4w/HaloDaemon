# Plugins

Plugins extend HaloDaemon with hardware support, integrations, and lighting
effects without compiling them into the daemon. They are distributed separately
from HaloDaemon, so support can be added or updated independently.

A plugin is a small package containing a `plugin.yaml` manifest and a Lua entry
script. The manifest describes the plugin, supported platforms, requested
permissions, capabilities, and allowed transports. The Lua code implements its
runtime behavior.

## Plugin types

- **Device plugins** add support for physical hardware such as RGB controllers,
  coolers, peripherals, sensors, and fan controllers. They declare how devices
  are discovered and expose capabilities such as RGB, fan control, LCD, DPI, or
  battery status.
- **Integration plugins** connect HaloDaemon to a host service or another
  application, such as Linux hwmon or an OpenRGB server. They can discover
  devices or expose data that is not tied to one directly matched USB or HID
  device.
- **Effect plugins** provide reusable RGB effects that can be selected by
  compatible lighting zones.

Plugins are disabled until explicitly enabled. HaloDaemon shows the permissions
and hardware or service access requested by a plugin before activation, then
limits it to those declared transports and resources at runtime.

## Plugin development

The [HaloDaemon plugins repository](https://github.com/TimP4w/HaloDaemon-plugins)
contains the official plugins, working package examples, and the authoritative
development documentation:

- [Plugin manifest reference](https://github.com/TimP4w/HaloDaemon-plugins/blob/main/docs/manifest-reference.md)
  — package metadata, plugin types, capabilities, permissions, device matching,
  transports, configuration, and effects.
- [Lua API and test harness](https://github.com/TimP4w/HaloDaemon-plugins/blob/main/docs/lua-api.md)
  — lifecycle callbacks, capability APIs, transports, sandbox behavior, and
  plugin tests.
- [Official plugin catalog](https://github.com/TimP4w/HaloDaemon-plugins#plugin-catalog)
  — package examples and links to the protocol documentation maintained with
  each plugin.

Use those references when creating or updating a plugin; this page only
describes how plugins fit into HaloDaemon.
