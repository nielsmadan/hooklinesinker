# 0002 — Share one active binary across consumers

**Status:** accepted
**Recorded:** 2026-09-11, extracted from the implemented consumer installation flow.

## Context

Juggler and ringleader can ship different hooklinesinker versions and update independently.
Hook commands must continue to work when either consumer upgrades or is removed. Pointing
hooks inside one consumer's application bundle would tie every consumer to that bundle's
location and lifecycle.

## Decision

Copy consumer-supplied binaries into a shared versioned installation and point all generated
hooks at its stable active symlink. Within one protocol major, activate a candidate only when
its version is strictly newer; reuse equal or newer active versions. Reject a protocol
mismatch rather than silently replacing an installation another consumer still depends on.

Keep consumer registrations separate from hook configuration. Removing a consumer preserves
the shared hooks and binary while another registration exists. When the last registration
leaves, remove owned hooks and the active symlink, while retaining version directories.

An incompatible protocol major is a coordinated migration, not an in-place upgrade. Stop
consumers, record the current registrations and hook agents, then unregister every old-major
consumer. The last removal clears owned hooks and the active symlink. Register the new-major
consumers with compatible binaries, reinstall hooks for each recorded agent from the new active
binary, and verify `version --json`, `consumers --json`, hook status, and `doctor`.

Do not mix majors behind one active symlink. To roll back, unregister every new-major consumer,
invoke a retained old-major binary to register the old consumers again, reinstall its hooks,
and repeat the same checks. Retained version directories provide rollback material but are
never selected automatically across a protocol-major boundary.

## Consequences

- Installation order cannot downgrade a compatible active helper. A consumer must support the
  protocol it registers for even when another consumer supplies the active version.
- Rebuilding a binary with the same version does not upgrade an active installation. Release
  changed binaries under a new version; use `just refresh-active` only for local iteration.
- Updating the active executable does not install new hook entries or refresh copied adapters.
  Hook reconciliation and host trust remain explicit steps.
- A major migration has a deliberate interruption between removing the old hooks and installing
  the new ones; consumers must coordinate that maintenance window.
- Deactivation does not erase all stored data. Old binaries, ledger/health data, and directories
  can remain after the last consumer is removed.

See [installation](../installation.md) for the current procedure and
[Installer](../../src/install.rs) for activation and removal behavior.
