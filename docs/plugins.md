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

## Distribution and repository trust

HaloDaemon installs complete repositories, not standalone plugin directories.
A repository can come from an HTTPS/SSH Git URL, a local `file://` Git checkout,
or a local `.tar`, `.tar.gz`, or `.tgz` archive. Every repository has a generated
`repository.yaml` index containing the package versions and SHA-256 digests;
packages whose contents do not match that index are never loaded.

The official repository is authenticated with keys built into HaloDaemon. A
third-party repository may optionally advertise one Ed25519 public key in its
index and provide `repository.sig`. On first import HaloDaemon verifies the
self-signature and pins that key (trust on first use). Every later revision must
verify with the pinned key; changing or adding a key requires removing and
importing the repository again. The UI displays the pinned key fingerprint.
Repositories without a key remain unsigned but still receive package-hash
validation. A signature proves continuity after the first import; it does not
independently prove the publisher's real-world identity.

### Signing a third-party repository

The public key is advertised in the repository root's `repository.yaml`, as a
top-level `signing_key` block (next to `compatibility`, not inside a package's
`plugin.yaml`):

```yaml
schema: 1
id: example-plugins
name: Example plugins
version: 2026.7.1

signing_key:
  id: example-repository-2026
  algorithm: ed25519
  public_key: "<base64-encoded raw 32-byte Ed25519 public key>"

compatibility:
  halod: ">=0.5.0"
  plugin_api: 2

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
# repository.sig over the exact canonical repository.yaml bytes.
cargo run --manifest-path src/Cargo.toml -p halod-plugin-signing -- \
  index /path/to/repository --version 2026.7.1
cargo run --manifest-path src/Cargo.toml -p halod-plugin-signing -- \
  sign /path/to/repository --key-id example-repository-2026
```

Commit both `repository.yaml` and `repository.sig`, but never the private seed.
The `key_id` in `repository.sig` must match `signing_key.id`. HaloDaemon verifies
this self-signature on first import, shows the public-key fingerprint, and pins
the key. A later public-key addition, removal, or replacement is rejected; key
rotation therefore requires users to remove and re-import the repository.

#### Testing a signed local repository

`--dev-plugin-repo` is an intentionally unverified development override. It
loads a working tree whose files and hashes may change on every edit, so adding
`signing_key` there does not change its UI provenance from **Development
source**. Do not use that flag to test signing or first-import trust.

To test the signed-local flow:

1. Run `index`, then `sign`, so package metadata/hashes, `signing_key`, and
   `repository.sig` all describe the same exact repository state.
2. Commit `repository.yaml`, `repository.sig`, and the indexed package changes.
   **Local Git folder imports the committed `HEAD`; it ignores uncommitted
   working-tree edits.** Alternatively, create and import a new signed archive.
3. Start HaloDaemon without `--dev-plugin-repo`, remove any earlier unsigned
   registration of this repository, and import it using **Local Git folder**.

An existing unsigned registration cannot silently become signed: the absence
of a key was part of its first-import trust state. Remove and re-import it after
signing. Likewise, copying a public key into `repository.yaml` is not enough;
without a matching `repository.sig` made by the corresponding private seed,
HaloDaemon cannot verify that the repository controls that key.

If import still fails, run `halod-plugin-signing validate <repo>` first. Fix any
package `id`, version, or SHA-256 mismatch before signing; changing any indexed
content after `sign` invalidates the detached signature.

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
