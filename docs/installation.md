# Installation and consumer ownership

There are three separate operations: put a CLI on `PATH`, activate a shared binary and
register a consumer, then install hooks in the selected agents. Consumers can ship their own
candidate binary; all registered consumers share the active installation.

## Activate a candidate

```sh
just install
hooklinesinker install --consumer me
hooklinesinker hooks install --agent claude
hooklinesinker sessions --json
```

[`just install`](../Justfile) uses Cargo to replace its installed command with the current
checkout. It neither activates that build nor changes agent configuration. The CLI's
[`install --consumer`](../src/main.rs) copies the executable being invoked into the shared
installation if promotion is needed, then writes the named consumer's registration.

[`Installer`](../src/install.rs) selects the active candidate as follows:

| Existing active installation | Result |
|---|---|
| None | Activate the candidate. |
| Same protocol, candidate is newer | Copy it and atomically switch the active symlink. |
| Same protocol, candidate is equal or older | Reuse the active binary without copying. |
| Different protocol major | Refuse installation before registering the consumer. |

The version comes from the candidate's compiled Cargo package version. Rebuilding an equal
version does not replace the shared binary; see [local iteration](development-and-releases.md).
Old version directories survive promotion. Activation happens before registration validation,
so an invalid consumer name or sink can fail after the binary has already been activated.

[`ConsumerStore`](../src/consumers.rs) replaces only the registration for the same name;
other names remain registered. Names start with a lowercase letter or digit and otherwise
contain lowercase letters, digits, `_` or `-`. The CLI requests the `status` capability with
its own protocol major. `--sink URL` enables HTTP(S) delivery; omitting it on re-registration
clears that consumer's sink. Use `consumers --json` to inspect registrations.

## Install or refresh hooks

`install --consumer` does not install or refresh hooks. Existing command hooks follow the
shared symlink when it changes, but new events, changed timeouts and embedded OpenCode/Pi
adapter source require a separate `hooks install --agent AGENT` using the updated helper.
The hook command's templates come from the executable invoked, so use the shared binary
when a bundled or Cargo-installed copy is older than the active installation.

[`HookManager`](../src/hooks.rs) reconciles owned entries and preserves foreign entries;
unsupported configuration shapes are reported and left alone. `hooks status --agent AGENT
--json` reports configuration state; it does not establish that the host trusts or has loaded
the hooks. Codex trust is handled by Codex's `/hooks` or the consumer's trust UI.
Hooklinesinker never edits Codex's `config.toml` or its `trusted_hash` entries.

See the [README path reference](../README.md#paths) for agent configuration locations and
[hooks and adapters](hooks-and-adapters.md) for ownership, migration and host-specific behavior.

## Remove a consumer

`uninstall --consumer me` removes that registration. While another consumer remains, hooks
and the active binary stay. Removing the last consumer attempts hook removal for every
supported agent, then removes the active symlink. An I/O failure stops cleanup at that point;
unsupported configurations remain untouched. Version directories, ledger/health files and
installation directories remain. [`Installer::uninstall_consumer`](../src/install.rs) owns
this sequence; [CLI tests](../tests/cli.rs) cover retention of version directories.

`hooks uninstall --agent AGENT` removes that agent's managed hooks independently of consumer
registration and therefore affects every consumer. `just uninstall` removes only Cargo's
installed command; it does not unregister consumers or remove the shared installation.

## Shared paths and isolated checks

[`paths.rs`](../src/paths.rs) puts binaries under `$XDG_DATA_HOME/hooklinesinker` and private
state under `$XDG_STATE_HOME/hooklinesinker`, defaulting to `~/.local/share` and
`~/.local/state`. Both tool roots are `0700`. The stable executable is
`bin/hooklinesinker`, a relative symlink into `versions/<version>/hooklinesinker`.
Consumers read state through the CLI's JSON commands; the files are private implementation.

Separate XDG data/state directories are sufficient for an isolated activation experiment
using only `install`, `version` and `consumers`. They do not relocate agent configuration:
last-consumer `uninstall` and `hooks install|uninstall` can still touch host configuration.
See the [shared-upgrade procedure](tests/shared-upgrade/README.md) for a bounded check.
