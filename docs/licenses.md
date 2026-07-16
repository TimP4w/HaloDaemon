# Licensing & attribution

HaloDaemon is **GPL-3.0-or-later** and follows the [REUSE](https://reuse.software/)
convention: every file's license is machine-declared, and full license texts live
in the top-level [`LICENSES/`](../LICENSES/) directory.

Licensing is tracked through **several independent mechanisms**, each with its own
"where it's declared" and "how it reaches the user". This document maps them so you
know what to touch when you add a device, a crate, or a bundled asset — and so an
audit has a single starting point.

## At a glance

| Track | What it covers | Declared in | Reaches the user via | Tool |
|-------|----------------|-------------|----------------------|------|
| REUSE / SPDX | First-party source + repo assets | [`REUSE.toml`](../REUSE.toml) + per-file SPDX headers + [`LICENSES/`](../LICENSES/) | The repo itself | `reuse lint` |
| Protocol attributions | Ported/adapted driver code | Per-file SPDX headers | README table + GUI *About* dialog | [`ui/build.rs`](../src/ui/build.rs) `scan_protocol_references()` |
| Rust crate deps | Every compiled dependency | [`about.toml`](../src/about.toml) `accepted` list | GUI *About* dialog | `cargo-about` (run by `build.rs`) |
| Bundled fonts and icons | `.ttf` and third-party `.svg` files embedded in the GUI/daemon | `REUSE.toml` overrides + SVG SPDX headers | GUI *About* dialog "Bundled assets" | `build.rs` |
| PawnIO blobs | Windows kernel-access `.bin` files | `REUSE.toml` `pwnio/**` | Windows installer (`PawnIO-LICENSE.txt`) | `stage-release.ps1` |
| External tools | FFmpeg (subprocess, bundled on Windows) | `packaging/windows/FFmpeg-*` + `REUSE.toml` | Windows installer (`ffmpeg.exe` + `FFmpeg-LICENSE.md`) | — |
| Official plugins | Signed Lua package snapshot embedded in release `halod` | Plugin manifests, SPDX headers, `LICENSES/`, and generated `licenses.txt` | About → Licenses, release archives, Windows installer, Linux packages | Plugin repo `scripts/generate-licenses.py` |

The single most complete artifact is the GUI **About → Licenses** dialog: it stitches
together protocol references, every Rust crate license, and the bundled asset licenses
at build time (see [`ui/build.rs`](../src/ui/build.rs)).

## 1. First-party source — SPDX headers + REUSE

REUSE requires every tracked file to have a copyright + license. This is satisfied by
the catch-all annotation in [`REUSE.toml`](../REUSE.toml):

```toml
[[annotations]]
path = "**"
precedence = "aggregate"
SPDX-FileCopyrightText = "Timucin Besken <beskent@gmail.com>"
SPDX-License-Identifier = "GPL-3.0-or-later"
```

Consequences:

- **A brand-new file needs nothing** — it inherits GPL-3.0-or-later automatically.
- A file that carries its **own** SPDX header (see §2) keeps that header; `aggregate`
  means the file's own info wins, the catch-all is only the fallback.
- `REUSE.toml` `override` annotations (fonts, PawnIO, FFmpeg docs) *replace* the
  catch-all for specific paths whose real owner isn't this project.

**Adding a new license:** drop its text at `LICENSES/<SPDX-id>.txt` (e.g.
`LICENSES/MPL-2.0.txt`). `reuse lint` fails on both *missing* licenses (referenced but
no file) **and** *unused* ones (file present but nothing references it), so keep this
directory in exact sync with what's actually declared.

**Verify:** `reuse lint` (the `reuse` tool ships in the Nix dev shell).

Currently declared license set: `GPL-3.0-or-later`, `GPL-2.0-or-later`,
`Apache-2.0`, `OFL-1.1`, and `LGPL-2.1-or-later` — and exactly those five texts
live in `LICENSES/`.

## 2. Ported / adapted driver code — per-file SPDX headers

When you port or adapt third-party code (a protocol decode, a register map), add a
REUSE-style header to the top of the file using the **upstream's actual license and
copyright holder** — not this project's:

```rust
// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project
```

Because the whole project is GPL-3.0-or-later, every upstream you pull from must be
GPL-3.0-**compatible** (GPL-2.0-or-later, GPL-3.0-or-later, MPL-2.0, MIT, BSD, …).
A GPL-2.0-**only** upstream would be incompatible — don't adapt from one.

These headers surface in two places:

1. **[README Acknowledgments](../README.md#acknowledgments)** — the human-facing source
   of truth. Add a row when you introduce a new upstream. Keep the license column equal
   to the file's SPDX header.
2. **The GUI About dialog** — [`ui/build.rs`](../src/ui/build.rs) `scan_protocol_references()`
   walks the Rust source roots for every workspace crate, reads the SPDX header, and groups non-project
   copyright holders by license under a "Protocol references" section and appends
   the corresponding full license texts from `LICENSES/`.

> Keep the file header, the README row, and (implicitly) the About dialog in agreement.
> If a file adapts an upstream, its SPDX license should equal that upstream's license —
> not default to GPL-3.0-or-later just because the project is.

## 3. Third-party Rust crates — cargo-about

Crate dependency licenses are **not** handled by REUSE or `LICENSES/`. They're handled
by [`cargo-about`](https://github.com/EmbarkStudios/cargo-about):

- [`about.toml`](../src/about.toml) lists every `accepted` SPDX license. A crate whose
  license isn't in this list fails the generation.
- [`about_licenses.hbs`](../src/about_licenses.hbs) is the output template.
- [`ui/build.rs`](../src/ui/build.rs) runs `cargo about generate about_licenses.hbs` at
  build time and embeds the result into `$OUT_DIR/third_party_licenses.txt`, shown in
  the About dialog under "Rust crate dependencies". cargo-about pulls each license
  **text from the crate itself**, so nothing needs to be copied into `LICENSES/`.
- In CI, `HALOD_REQUIRE_LICENSES=1` makes the build **fail** if cargo-about can't run,
  so release binaries never ship without the dependency license list. Local builds fall
  back to a placeholder with a warning.

**Adding a crate with a new license:** add its SPDX id to `about.toml` `accepted`, then
regenerate. This is orthogonal to §1 — e.g. many crates are MIT, but no first-party
file is, so there is deliberately **no** `LICENSES/MIT.txt`.

## 4. Bundled fonts — OFL-1.1

All embedded fonts live in one place, [`src/assets/fonts/`](../src/assets/fonts/),
shared by the daemon (LCD text rendering) and the GUI (egui):

- NotoSans, JetBrains Mono, Inter Tight — all **OFL-1.1**.
- Attributed via `override` annotations in [`REUSE.toml`](../REUSE.toml).
- Loaded with `include_bytes!` from [`daemon/src/lcd/engine/templates.rs`](../src/daemon/src/lcd/engine/templates.rs)
  and [`ui/src/ui/theme.rs`](../src/ui/src/ui/theme.rs).
- Credited in the About dialog "Bundled assets" section, which ships the full OFL-1.1
  text (see `append_bundled_asset_licenses()` in [`ui/build.rs`](../src/ui/build.rs)).

## 4a. Bundled icons — Apache-2.0

The GUI embeds a small SVG icon set from [`ui/assets/icons/`](../src/ui/assets/icons/).
Most glyphs are original HaloDaemon assets and inherit the project license. The SVGs
that came from Pictogrammers Material Design Icons or Google Material Icons carry
their own `Apache-2.0` SPDX header and copyright attribution. The About dialog names
both icon projects and includes the full Apache-2.0 text from `LICENSES/`.

## 5. PawnIO kernel blobs — LGPL-2.1-or-later

Windows low-level hardware access (chipset SMBus, SuperIO, AMD SMN) uses prebuilt
[PawnIO](https://github.com/namazso/PawnIO_modules) modules in [`pwnio/`](../pwnio/):
`SmbusI801.bin`, `SmbusPIIX4.bin`, `LpcIO.bin`, `AMDFamily17.bin` (© 2023 namazso,
**LGPL-2.1-or-later**), plus their `COPYING`.

- The whole directory is covered by `REUSE.toml`'s `pwnio/**` override (use a glob so
  new blobs are covered automatically).
- [`packaging/windows/stage-release.ps1`](../packaging/windows/stage-release.ps1) copies the blobs into
  the installer and ships `COPYING` as `PawnIO-LICENSE.txt` beside the binaries.

## 6. External runtime tools — FFmpeg

The LCD **video** feature ([`daemon/src/lcd/engine/video.rs`](../src/daemon/src/lcd/engine/video.rs))
shells out to an `ffmpeg` **subprocess** — it is not linked. The bundled Windows build
is a **GPL** build of FFmpeg (GPL-3.0-compatible with this project).

- On Windows the daemon prefers an `ffmpeg.exe` **next to the binary**, else `ffmpeg`
  from `PATH`.
- [`packaging/windows/stage-release.ps1`](../packaging/windows/stage-release.ps1) stages `ffmpeg.exe`
  from MSYS2 UCRT64 next to `halod.exe`, walks its dependencies with `ntldd` and copies
  every required non-system runtime DLL, and ships [`packaging/windows/FFmpeg-LICENSE.md`](../packaging/windows/FFmpeg-LICENSE.md)
  and `FFmpeg-README.txt` beside them.
- The dependency walk includes separately licensed codec/runtime libraries. Staging
  resolves every copied file back to its exact MSYS2 package, writes the package
  versions plus FFmpeg's build configuration/source links to
  `ThirdPartyLicenses/MSYS2/MSYS2-PACKAGES.txt`, and copies every license directory
  supplied by those packages. An unowned DLL fails staging instead of silently
  entering the installer.
- `FFmpeg-LICENSE.md` is only FFmpeg's licensing **summary** — it points at `COPYING.*`
  texts for the operative terms. Because the bundled binary is a GPL (version3) build
  with an LGPL core, `stage-release.ps1` also copies the repo's
  [`LICENSES/GPL-3.0-or-later.txt`](../LICENSES/GPL-3.0-or-later.txt) and
  [`LICENSES/LGPL-2.1-or-later.txt`](../LICENSES/LGPL-2.1-or-later.txt) into the staging
  tree as `COPYING.GPLv3` / `COPYING.LGPLv2.1` (of the four `COPYING.*` names the
  summary references, the two operative for this build), so the full GPL/LGPL texts
  are actually conveyed with the binary.
- `FFmpeg-LICENSE.md` / `FFmpeg-README.txt` are FFmpeg's own, attributed to the FFmpeg
  developers via a `REUSE.toml` override (not this project).
- The installer's wizard notice ([`packaging/windows/LICENSE.txt`](../packaging/windows/LICENSE.txt))
  lists FFmpeg under "Bundled components".

> **Keep in sync:** because it's a **GPL** ffmpeg build, its DLLs are copied too — if
> the MSYS2 package is swapped for a differently-licensed build, revisit the notice in
> `LICENSE.txt` and this section. `stage-release.ps1` enforces this at staging time:
> it checks `ffmpeg -version`, **fails** on `--enable-nonfree` (unredistributable) and
> **warns** if `--enable-gpl` / `--enable-version3` disappear from the build config.

## 7. Windows installer — Inno Setup

[`packaging/windows/halod.iss`](../packaging/windows/halod.iss) packages the staged tree
([`stage-release.ps1`](../packaging/windows/stage-release.ps1) output):

- `LicenseFile=staging/INSTALLER-LICENSE.txt` — the HaloDaemon notice plus the
  generated per-plugin/SPDX license list shown in the install wizard.
- `InfoBeforeFile=DISCLAIMER.txt` — the pre-install disclaimer.
- The staged tree carries the three HaloDaemon executables, the PawnIO blobs, and
  `PawnIO-LICENSE.txt`.
- The **full** third-party license text (crates + protocol references + fonts/icons) travels
  inside the binary and is viewable at runtime in **About → Licenses**.

## 8. Embedded official plugins

Official release builds embed a deterministic, signed snapshot of the official
Lua plugin repository. The release workflow validates the selected repository
commit, packages its scripts, `REUSE.toml`, and complete `LICENSES/` directory,
and passes the archive to the daemon build. At first launch the daemon validates
the signature and indexed package hashes again before extracting any script.

The plugin publication workflow generates `licenses.txt` from every manifest's
declared plugin license and every SPDX license/copyright found inside that
package. Plugin CI rejects a stale notice. HaloDaemon verifies that file and
its SHA-256 through the signed repository index, then copies the plugin-owned
artifact into each build/package; no generated copy is kept in the HaloDaemon
repository.

The GUI appends the notice to **About → Licenses**. Windows shows it on the
installer license page and installs it under `ThirdPartyLicenses/Plugins`;
Linux tarballs, debs, RPMs, Arch, and Nix install it under their normal shared
license path. The same source tree and license texts also travel inside the
plugin bundle embedded in `halod`.

## Reference clones — `refs/`

[`refs/`](../refs/) holds full clones of upstream projects (e.g. LibreHardwareMonitor)
used purely as a reading reference while porting. It is **git-ignored**, never shipped,
and therefore intentionally outside REUSE's scope — no annotation needed.

## Checklist — keeping licensing correct when you change things

- **New Rust dependency** → confirm its license is in [`about.toml`](../src/about.toml)
  `accepted`; add the SPDX id if it's new. Run `cargo about generate about_licenses.hbs`
  from `src/` to confirm it resolves.
- **Ported protocol/driver code** → add an SPDX header with the *upstream's* license +
  copyright; add a [README Acknowledgments](../README.md#acknowledgments) row; ensure
  `LICENSES/<id>.txt` exists.
- **New bundled asset** (font, blob, data file this project doesn't own) → add a
  `REUSE.toml` `override` with the real owner + license, and make sure it's shipped with
  its license text in whatever package carries it.
- **Always** run `reuse lint` (first-party) before a release; CI enforces the crate
  side via `HALOD_REQUIRE_LICENSES=1`.
