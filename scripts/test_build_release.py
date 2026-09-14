import os
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("build-release.sh")
TARGET = "x86_64-unknown-linux-gnu"


class BuildReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.repo = Path(self.temp.name)
        (self.repo / "scripts").mkdir()
        (self.repo / "scripts" / "build-release.sh").write_bytes(SCRIPT.read_bytes())
        (self.repo / "dist").mkdir()
        (self.repo / "dist" / "existing").write_text("keep")
        self.bin = self.repo / "bin"
        self.bin.mkdir()

    def tearDown(self):
        self.temp.cleanup()

    def command(self, name, body):
        path = self.bin / name
        path.write_text(f"#!/bin/bash\nset -euo pipefail\n{body}\n")
        path.chmod(0o755)

    def run_script(self, target=TARGET):
        env = os.environ.copy()
        env["PATH"] = f"{self.bin}:{env['PATH']}"
        return subprocess.run(
            ["bash", str(self.repo / "scripts" / "build-release.sh"), target],
            cwd=self.repo,
            env=env,
            text=True,
            capture_output=True,
        )

    def test_unsupported_target_preserves_existing_dist(self):
        result = self.run_script("unsupported-target")

        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((self.repo / "dist" / "existing").read_text(), "keep")

    def test_build_failure_preserves_existing_dist(self):
        self.command("rustup", f'echo "{TARGET}"')
        self.command("cargo", "exit 1")

        result = self.run_script()

        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((self.repo / "dist" / "existing").read_text(), "keep")

    def test_success_replaces_dist_after_artifacts_are_ready(self):
        self.command("rustup", f'echo "{TARGET}"')
        self.command(
            "cargo",
            """
target=""
while [ "$#" -gt 0 ]; do
    if [ "$1" = "--target" ]; then
        target="$2"
        break
    fi
    shift
done
mkdir -p "$PWD/target/$target/release"
printf new >"$PWD/target/$target/release/hooklinesinker"
""",
        )

        result = self.run_script()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.repo / "dist" / "hooklinesinker-linux-x86_64").read_text(),
            "new",
        )
        self.assertTrue((self.repo / "dist" / "SHA256SUMS").exists())


if __name__ == "__main__":
    unittest.main()
