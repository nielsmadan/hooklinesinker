# 0005 — Home-relative spellings of the active binary count as installed

**Status:** accepted
**Recorded:** 2026-10-30, with home-relative hook recognition.

## Context

The original plan (`docs/plans/2026-08-29-hooklinesinker.md`) put an expanded absolute path to
the active binary in every hook, and installs converged any other spelling to it. A user who
renders agent configuration from a config manager writes the same hooks with home-relative paths:
`~/.local/share/hooklinesinker/bin/hooklinesinker ingest ...` in JSON hook commands, and
`join(homedir(), ".local/share/hooklinesinker/bin/hooklinesinker")` in the OpenCode and Pi
adapters. The config manager treats any change to its rendered bytes as a hand edit. Each
consumer registration ran `hooks install`, which rewrote those files to absolute paths, re-sorted
every key and reset modes, so the two writers kept overwriting each other.

## Decision

A spelling that resolves to the active binary counts as installed, and install leaves it as
written:

- JSON hosts known to run hook commands through a POSIX shell accept a binary word starting with
  `~/` or `$HOME/` (`HookTarget::accepts_home_relative_commands`). Claude and Codex were observed
  recording status from `~/` commands, Qwen's Unix hook runner spawns `bash -c`, and Droid's hooks
  documentation describes hooks as shell commands with a `~/` example.
- An OpenCode or Pi adapter that is byte-identical to the template with
  `join(homedir(), "<path relative to $HOME>")` as the binary expression counts as installed.
- Install leaves an installed JSON event, Kimi file or adapter untouched, and JSON rewrites keep
  key order and file mode.

Fresh installs still write the absolute path, so a host whose `$HOME` differs from the
installer's (sudo, some sandboxes) still reaches the binary. Kimi keeps exact canonical matching:
one malformed `[[hooks]]` entry disables its whole config, and how it spawns hook commands is not
verified. An installed adapter that is group- or world-writable is still rewritten at `0600`, and
a writable plugin directory is still refused.

Rejected: writing home-relative paths by default (it changes every user's hooks to depend on the
agent's `$HOME`); having the config manager normalize hooklinesinker's spellings (its check must
compare the bytes it rendered); a print-only mode for external managers (deferred to the user).

## Consequences

- A home-relative entry resolves against the agent's `$HOME` at run time while status compares
  against the installer's. An agent run under a different `$HOME` sees a hook that status reports
  installed but that cannot reach the binary.
- An external renderer must reproduce the adapter template and marker line byte for byte and
  follow template changes between releases; otherwise the file is drifted and rewritten with the
  absolute path.
- Adding a JSON host means deciding, from evidence, whether it accepts home-relative commands.
