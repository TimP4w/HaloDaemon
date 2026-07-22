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
- **LCD plugins** contribute programmable widgets and declarative
  layout presets to the custom-screen editor.

Plugins are disabled until explicitly enabled. HaloDaemon shows the permissions
and hardware or service access requested by a plugin before activation, then
limits it to those declared transports and resources at runtime.

## Distribution and release trust

A plugin is standalone. Publishers may bundle one or several plugins as a pack
in an immutable GitHub release. HaloDaemon reads only three release assets:
generated `release.yaml`, detached `release.sig`, and `plugins.tar.gz`. It never
clones the source repository and has no legacy manifest fallback.

`release.yaml` contains the release identity, package versions and SHA-256
digests, and the exact archive name, size, and digest. HaloDaemon validates the
whole pack before atomically activating it. Users follow the newest stable
release by default and can pin an older release tag from the source selector.

The official source is authenticated with keys built into HaloDaemon. A
third-party source may advertise an Ed25519 public key; HaloDaemon verifies
`release.sig` and pins the key on first import. Every later release must verify
with that key. Unsigned sources still receive package and archive hash checks.

### Signing a third-party release

The public key is a top-level `signing_key` block in `release.yaml`:

```yaml
schema: 1
id: example-plugins
name: Example plugins
version: 2026.7.1

signing_key:
  id: example-repository-2026
  algorithm: ed25519
  public_key: "<base64-encoded raw 32-byte Ed25519 public key>"

packages: []
```

`public_key` is the standard Base64 encoding of the raw 32-byte Ed25519 public
key. It is not a PEM document or a file path. The matching private 32-byte seed
must remain outside the repository.

The signing tool writes this block automatically, so manual editing is normally
unnecessary:

```sh
# Run once and store private_seed_b64 in a secret manager or CI secret.
cargo run --manifest-path src/Cargo.toml -p halod-plugin-signing -- \
  keygen example-repository-2026

export HALOD_PLUGIN_SIGNING_KEY_B64='<private_seed_b64>'

# Refresh package versions/hashes, advertise the derived public key, and write
# release.sig over the exact canonical release.yaml bytes.
cargo run --manifest-path src/Cargo.toml -p halod-plugin-signing -- \
  index /path/to/repository --version 2026.7.1
cargo run --manifest-path src/Cargo.toml -p halod-plugin-signing -- \
  sign /path/to/repository --key-id example-repository-2026
```

Publish `release.yaml`, `release.sig`, and `plugins.tar.gz` as assets on the same
GitHub release. Never publish the private seed. The signature key id must match
`signing_key.id`; changing the pinned key requires removing and re-adding the
source. Run `halod-plugin-signing validate <pack>` before publication.

## Plugin development

The [HaloDaemon plugins repository](https://github.com/TimP4w/HaloDaemon-plugins)
contains the official plugins, working package examples, and the authoritative
development documentation:

- [Plugin manifest reference](https://github.com/TimP4w/HaloDaemon-plugins/blob/main/docs/manifest-reference.md)
  covers package metadata, plugin types, capabilities, permissions, device matching,
  transports, configuration, effects, widgets, and presets.
- [Lua API and test harness](https://github.com/TimP4w/HaloDaemon-plugins/blob/main/docs/lua-api.md)
  covers lifecycle callbacks, capability APIs, transports, sandbox behavior,
  effect/widget callbacks, and plugin tests.
- [Official plugin catalog](https://github.com/TimP4w/HaloDaemon-plugins#plugin-catalog)
  covers package examples and links to the protocol documentation maintained with
  each plugin.

Use those references when creating or updating a plugin; this page only
describes how plugins fit into HaloDaemon.
