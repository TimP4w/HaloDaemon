# AGENTS.md

This file provides guidance to all coding agents working in this repository.
Keep it focused on durable rules and concepts. Do not add references to specific
source files, symbols, or line numbers; those change frequently. Use repository
search and the maintained documentation to locate current implementations.

## Before claiming completion or committing

Always use the Nix development environment. Before claiming work is complete
or committing, run from the repository root and resolve every issue:

- `nix develop --command bash -c "cd src && cargo fmt --all -- --check"`
- `nix develop --command bash -c "cd src && cargo test --workspace"`
- `nix develop --command bash -c "cd src && cargo clippy --workspace --all-targets -- -D warnings"`

Do not claim completion if any required check was skipped or failed. Report the
exact blocker instead.

## Commit format

Use Conventional Commits: `<type>(<optional scope>): <summary>`.

- `feat`: new feature
- `fix`: bug fix
- `docs`: documentation only
- `chore`: CI/CD, packaging, tooling, dependencies, formatting, or build changes
- `refactor`: code change that neither fixes a bug nor adds a feature
- `test`: adding or fixing tests only
- `perf`: performance improvement
- `revert`: reverting a previous commit

A scope is encouraged for changes confined to one domain.

## Code conventions

- **Minimize code.** Prefer the smallest maintainable change. Reuse existing
  helpers, traits, accessors, use cases, and transports instead of creating
  parallel abstractions.
- **Keep comments sparse.** Encode non-obvious behavior in clear names and tests.
- **Test all new behavior, including GUI behavior.** Factor logic around
  immediate-mode painting into pure functions over plain data and unit-test it.
- **Prefer meaningful property tests.** Good invariants include round-trip
  identity, bounded output, valid permutations, and preserved monotonicity.
- **Keep layers separate.** Protocol code handles wire formats, devices expose
  capabilities, transports move bytes, use cases coordinate behavior, and the
  GUI presents state. Do not leak responsibilities across these boundaries.
- **Do not place device assumptions in generic code.** Device-specific timing,
  caching, keepalive, and duplicate-write behavior must be declared by the
  device or capability. Generic behavior defaults to the safe option.
- **Preserve user work.** The worktree may already be dirty. Do not overwrite,
  discard, reformat, or stage unrelated changes.
- **Attribute adapted code.** Follow REUSE and SPDX requirements whenever code
  or assets are ported from another project.

## Build and test

The Rust workspace is under `src/`. Run Cargo commands there and enter through
`nix develop` when not already inside the development shell.

Common commands:

- Build everything: `cargo build --workspace`
- Test everything: `cargo test --workspace`
- Test one crate: `cargo test -p <crate>`
- Run one test: `cargo test -p <crate> <test-name>`
- Run the daemon or GUI with `cargo run -p <crate>`
- Check coverage gaps with `cargo mutants`

A surviving mutant indicates missing coverage. Add a test that catches the
behavioral change instead of merely rerunning the tool.

Read the development documentation before adding hardware support or changing
platform integration. Read the architecture documentation before implementing
a feature that crosses domains or process boundaries. Use semantic/code-graph
search when available, but verify results against the current source.

## Architecture

The workspace contains shared types and algorithms, low-level hardware access,
a privileged Windows broker, an unprivileged daemon, and an eframe/egui GUI.

The daemon and GUI communicate through typed IPC. Commands are deserialized at
the boundary and dispatched to domain-owned use cases. High-frequency state is
pushed through subscriptions rather than request/response polling.

Hardware follows a vendor → device → protocol → transport split:

- Devices declare capabilities and expose capability interfaces.
- Transports move bytes without owning device behavior.
- Protocols encode and decode vendor wire formats.
- Controllers may discover child devices.
- Shared engines and use cases operate only on declared capabilities.

New device support normally belongs in the plugin repository as a Lua package.
Change the Rust driver stack only when plugins require a new core capability or
scoped transport primitive. Hardware changes also require appropriate Linux
access rules, protocol or transport documentation, and focused tests.

The GUI is capability-driven and contains no device logic. Keep pure derivations
from daemon state separate from painting. Keep high-frequency preview streams
separate from ordinary state synchronization.

Engines belong to their owning domain. Shared run-loop infrastructure may be
reused, but domain behavior should not accumulate in a generic engine module.

Configuration is split by concern into YAML files plus managed media. Preserve
atomic persistence behavior, backwards-compatible defaults, and safe filename
handling when changing configuration.

## Licensing and attribution

The workspace is GPL-3.0-or-later and follows REUSE. Every source or asset must
have an appropriate license declaration, and required license texts must be
present in the repository.

When adapting third-party material:

1. Add the correct SPDX license and copyright information.
2. Record the upstream source.
3. Add any missing license text.
4. Update dependency-license metadata when dependencies change.

Read the licensing documentation before changing REUSE configuration,
dependency-license configuration, bundled assets, or packaging.

## Lint policy

CI treats Clippy warnings as errors. Workspace-level lint exceptions exist for
intentional scaffolding and closure-heavy APIs; do not turn those exceptions
back into hard errors without understanding the repository-wide impact.
