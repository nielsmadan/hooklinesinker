# Shared binary upgrade check

Use this when changing activation or preparing a release intended to replace an already
installed helper. It exercises two real binaries against isolated data and state roots.
The normal Rust tests cover synthetic candidates; this check also confirms the versions
reported by the packaged executables.

## Inputs

Run from a checkout in a POSIX shell with Python 3. Set `HLS_PREVIOUS` and `HLS_CANDIDATE` to
verified executable paths for two different versions with the same protocol major. The
candidate version must be newer. For downloaded release binaries, verify `SHA256SUMS` first.
Record the executable hashes, versions, and source revisions alongside the result.

## Procedure

The subshell confines the XDG overrides to this procedure. `install`, `version`, and
`consumers` do not edit agent configurations. Do not run `hooks install` or `uninstall` here:
XDG isolation does not isolate the host's agent configuration paths.

```sh
(
  set -eu
  upgrade_root="$(mktemp -d)"
  export XDG_DATA_HOME="$upgrade_root/data"
  export XDG_STATE_HOME="$upgrade_root/state"
  active="$XDG_DATA_HOME/hooklinesinker/bin/hooklinesinker"
  printf 'Evidence directory: %s\n' "$upgrade_root"

  "$HLS_PREVIOUS" version --json > "$upgrade_root/previous.json"
  "$HLS_CANDIDATE" version --json > "$upgrade_root/candidate.json"
  "$HLS_PREVIOUS" install --consumer upgrade-check
  "$active" version --json > "$upgrade_root/before.json"
  "$HLS_CANDIDATE" install --consumer upgrade-check
  "$active" version --json > "$upgrade_root/after.json"
  "$active" consumers --json > "$upgrade_root/consumers.json"
  "$HLS_PREVIOUS" install --consumer older-consumer
  "$active" version --json > "$upgrade_root/reused.json"
  "$active" consumers --json > "$upgrade_root/both-consumers.json"

  python3 - "$upgrade_root" <<'PY'
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
def read(name):
    return json.loads((root / name).read_text())

previous, candidate = read("previous.json"), read("candidate.json")
assert previous["protocol"] == candidate["protocol"]
assert tuple(map(int, previous["version"].split("."))) < tuple(map(int, candidate["version"].split(".")))
assert read("before.json") == previous
assert read("after.json") == read("reused.json") == candidate
assert [c["name"] for c in read("consumers.json")["consumers"]] == ["upgrade-check"]
assert {c["name"] for c in read("both-consumers.json")["consumers"]} == {"upgrade-check", "older-consumer"}
print("Upgrade, consumer preservation, and older-version reuse passed")
PY
)
```

Keep the printed evidence directory until the result is recorded, then delete that temporary
directory. This does not test real agent hooks, consumer UIs, signing, or release downloads.

## Recorded runs

- [1.0.0 to 1.0.1 upgrade](runs/undated-1.0.0-to-1.0.1.md), recovered from the implementation
  conversation. The original invocation was not retained; it predates the procedure above.
