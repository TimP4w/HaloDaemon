# Command Transport

A scoped executable runner for plugins that read or control a device through a
vendor command-line tool, such as querying and setting an NVIDIA GPU with
`nvidia-smi` / `nvidia-settings`.

**Platform:** Linux and Windows

## Overview

A device matched by a `command` transport receives a `command` userdata that can
run only the executables the manifest declares. There is no shell: each call
spawns one process directly with an argument list, so nothing the plugin passes
is ever interpreted as a shell expression.

## Operations for plugins

| Operation | Purpose |
|---|---|
| `command.run(executable, args)` | Run a declared executable with an argument list and return its result. |

The result table carries `success`, `exit_code`, `stdout`, `stderr`, and
`timed_out`. Output is bounded, and a process that exceeds its output or time
limit fails rather than blocking the worker.

## Scope

The manifest lists the exact executables the plugin may run:

```yaml
permissions: [command]
transports:
  command:
    commands: [nvidia-smi]
```

Entries must be bare executable names resolved on `PATH`; a path, a shell
fragment, or an argument is runtime data, not authority, and is rejected. A
`match.command` value must appear in this list. Shells, interpreters, and
command launchers are refused even when allowlisted, and any executable outside
the declared set is a hard error.

## Limitations

- Only the declared executables can be run, with bounded arguments and output.
- No shell, pipes, redirection, or environment control.
- The plugin cannot spawn a shell or interpreter to escape the allowlist.

See the plugin repository's manifest reference (`transports.command`) and Lua API
(`command.run`) for the full authoring contract.
