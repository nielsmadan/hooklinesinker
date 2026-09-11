# Project documentation

Start with the [README](../README.md) for commands, protocol examples, and consumer integration.
These documents explain the flows and constraints behind that interface.

| When you need to… | Read |
|---|---|
| Understand stale sessions, binding identity, or sink recovery | [Status lifecycle](status-lifecycle.md) |
| Change agent hooks or the embedded TypeScript adapters | [Hooks and adapters](hooks-and-adapters.md) |
| Integrate a consumer or change activation and removal | [Installation](installation.md) |
| Set up checks, ship a release, or update a consumer's pin | [Development and releases](development-and-releases.md) |
| Understand why the protocol exposes only status | [Status-only decision](decisions/0001-status-events-only.md) |
| Understand why consumers share one active version | [Shared installation decision](decisions/0002-shared-versioned-installation.md) |
| Repeat an upgrade check with real release binaries | [Shared upgrade procedure](tests/shared-upgrade/README.md) |

Flow documents track the current code. Decision records preserve rationale; superseding one
requires a new ADR and a link from the old record. Manual test procedures stay current, while
their dated results record only what was observed in that run.
Locally retained implementation plans are historical references.
