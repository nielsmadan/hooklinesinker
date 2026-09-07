import re
import subprocess
import sys
from pathlib import Path

path = Path("Cargo.toml")
content = path.read_text()
package = re.search(r"(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)", content)
if package is None:
    raise SystemExit("Cargo.toml has no [package] section.")
updated, count = re.subn(
    r'^version = "[^"]+"$',
    f'version = "{sys.argv[1]}"',
    package[1],
    flags=re.MULTILINE,
)
if count != 1:
    raise SystemExit("Expected one version in Cargo.toml's [package] section.")
path.write_text(content[: package.start(1)] + updated + content[package.end(1) :])
subprocess.run(
    ["cargo", "metadata", "--offline", "--format-version", "1"],
    stdout=subprocess.DEVNULL,
    check=True,
)
