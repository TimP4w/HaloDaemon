# Plugins

Plugins extend HaloDaemon with hardware support, integrations, and lighting
effects without compiling them into the daemon. They are distributed separately
from HaloDaemon, so support can be added or updated independently.

A plugin is a small package containing a `plugin.yaml` manifest and a Lua entry
script. The manifest describes the plugin, supported platforms, the plugin API
it targets, capabilities, and allowed transports. The Lua code implements its
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
- **LCD plugins** contribute programmable widgets and declarative
  layout presets to the custom-screen editor.

A plugin can only reach what its manifest declares: its matched devices, its
SMBus addresses, the commands it lists, or its network origins. HaloDaemon shows
the user what each plugin uses and enforces those limits at runtime.

## Repositories and catalogs

A repository is either a catalog URL or a local folder. HaloDaemon installs,
updates and uninstalls plugins one at a time. A newly installed plugin stays
off until the user confirms what it uses; an update that would use more than
the installed version is installed disabled and the user is told, unless the
GUI already asked and the install carried that confirmation.

A remote repository publishes static files, for example on GitHub Pages:

```text
catalog.yaml                     signature line + index of every package
licenses.txt
packages/<id>-<version>.tar.gz   one immutable archive per package version
```

`catalog.yaml` (schema 2) lists every package version with its plugin `api`,
archive path, size and SHA-256, plus its display `name`, the hardware it
`matches` (concrete HID and USB ids, a `hid_any` flag for plugins that match
every HID device, GPU SMBus PCI ids, and command executables) and what it
`uses`, all derived from `plugin.yaml` by the same code the daemon loads
manifests with. These let a fresh install recommend plugins for connected
hardware and let the GUI show what an update would use before installing it.
HaloDaemon picks the newest version whose API it supports.
The catalog keeps the newest version for each API it has published, so an
older HaloDaemon still finds a compatible build. Archives are downloaded over
HTTPS without redirects, checked against the catalog, and swapped into place
atomically; a package whose `plugin.yaml` declares a different name, hardware
or uses than its catalog entry is refused, and a plugin that fails to load
after an update is rolled back.

The official repository is authenticated with keys built into HaloDaemon. A
third-party catalog may be signed: HaloDaemon pins its key the first time the
repository is added and rejects any catalog signed by another key, or not
signed at all. Unsigned catalogs are shown as unsigned. A catalog older than
the one already downloaded is rejected.

A local folder is scanned in place and wins over an installed plugin with the
same id, which makes it the way to develop a plugin against a running daemon.

### Publishing a catalog

The plugin repositories publish with a reusable workflow; see the
[plugins repository README](https://github.com/TimP4w/HaloDaemon-plugins#publishing).
The tool can also be run directly:

```sh
# Once: store private_seed_b64 as a CI secret, never in the repository.
cargo run --manifest-path src/Cargo.toml -p halod-plugin-signing -- keygen example-2026

cargo run --manifest-path src/Cargo.toml -p halod-plugin-signing -- \
  catalog /path/to/repository --out site --name "Example plugins" \
  --previous https://example.github.io/plugins/catalog.yaml \
  --trusted-key example-2026=<public_key_b64>
HALOD_PLUGIN_SIGNING_KEY_B64='<private_seed_b64>' \
  cargo run --manifest-path src/Cargo.toml -p halod-plugin-signing -- \
  sign site --key-id example-2026
cargo run --manifest-path src/Cargo.toml -p halod-plugin-signing -- \
  verify site --trusted-key example-2026=<public_key_b64>
```

`--previous` downloads and verifies the published catalog, carries older-API
entries forward, and fails if a package changed without a version bump. Use
`--bootstrap` instead for the first publish. Upload the whole `site` directory.

## Plugin development

The [HaloDaemon plugins repository](https://github.com/TimP4w/HaloDaemon-plugins)
contains the official plugins, working package examples, and the authoritative
development documentation:

- [Plugin manifest reference](https://github.com/TimP4w/HaloDaemon-plugins/blob/main/docs/manifest-reference.md)
  covers package metadata, plugin types, the plugin API version, capabilities, device matching,
  transports, configuration, effects, widgets, and presets.
- [Lua API and test harness](https://github.com/TimP4w/HaloDaemon-plugins/blob/main/docs/lua-api.md)
  covers lifecycle callbacks, capability APIs, transports, sandbox behavior,
  effect/widget callbacks, and plugin tests.
- [Official plugin catalog](https://github.com/TimP4w/HaloDaemon-plugins#plugin-catalog)
  covers package examples and links to the protocol documentation maintained with
  each plugin.

Use those references when creating or updating a plugin; this page only
describes how plugins fit into HaloDaemon.
