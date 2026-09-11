# Upgrade from 1.0.0 to 1.0.1

**Captured:** 2026-09-11 from the implementation conversation.
**Run date and exact time:** not retained in the available execution summary.

## Setup and purpose

The shared installer reuses an equal version. This check verified that a prepared 1.0.1 binary
could replace an existing 1.0.0 installation while preserving its consumer registration.
The reported setup used isolated XDG data and state roots on the development Mac.

The previous and candidate versions were 1.0.0 and 1.0.1, both protocol 1. Exact binary hashes,
source revisions, local patch, tool versions, temporary paths, consumer name, and original
commands were not retained. The [current procedure](../README.md) was written later and must
not be treated as the original invocation.

## Observed result

The execution summary reports successful promotion to 1.0.1, protocol 1, and preservation of
the registered consumer. The numeric process exit status and raw output were not retained.

This supports the shared-version upgrade behavior for that pair of binaries. It does not
establish hook reinstallation, Codex trust, application integration, or the older-version reuse
step added to the current procedure. The documentation pass did not rerun the experiment.
