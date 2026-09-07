import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("doctor.sh")
NODE = shutil.which("node")


class DoctorTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.log = self.root / "calls.jsonl"
        self.env = dict(
            os.environ,
            PATH=str(self.root) + os.pathsep + os.environ["PATH"],
            DOCTOR_TEST_LOG=str(self.log),
            DOCTOR_TEST_NODE="24.0.0",
            DOCTOR_TEST_HOOKS="0",
        )
        for name in ["cargo", "rustc", "python3", "node", "lefthook"]:
            path = self.root / name
            path.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\n"
                "from pathlib import Path\n"
                "name = Path(sys.argv[0]).name\n"
                "with open(os.environ['DOCTOR_TEST_LOG'], 'a') as log:\n"
                "    log.write(json.dumps([name, *sys.argv[1:]]) + '\\n')\n"
                "if name == 'node':\n"
                "    version = json.dumps(os.environ['DOCTOR_TEST_NODE'])\n"
                '    code = "Object.defineProperty(process.versions, \'node\', {value: " + version + "});" + sys.argv[-1]\n'
                f"    os.execv({NODE!r}, [{NODE!r}, '-e', code])\n"
                "if name == 'lefthook' and sys.argv[1:] == ['check-install']:\n"
                "    sys.exit(int(os.environ['DOCTOR_TEST_HOOKS']))\n"
            )
            path.chmod(0o755)

    def invoke(self):
        return subprocess.run(
            ["/bin/bash", str(SCRIPT)],
            cwd=self.root,
            env=self.env,
            capture_output=True,
            text=True,
        )

    def test_accepts_ready_checkout_without_installing_or_testing(self):
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for name in [
            "cargo",
            "rustc",
            "rustfmt",
            "clippy",
            "Python 3.9+",
            "Node 22.6+",
            "lefthook",
            "Git hooks",
        ]:
            self.assertIn(f"ok       {name}", result.stdout)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertEqual(
            calls[:4],
            [
                ["cargo", "--version"],
                ["rustc", "--version"],
                ["cargo", "fmt", "--version"],
                ["cargo", "clippy", "--version"],
            ],
        )
        self.assertEqual(calls[-2:], [["lefthook", "version"], ["lefthook", "check-install"]])
        self.assertEqual(len(calls), 8)

    def test_checks_node_version_at_the_adapter_boundary(self):
        for version, expected in [("20.20.0", 1), ("22.5.0", 1), ("22.6.0", 0), ("24.0.0", 0)]:
            with self.subTest(version=version):
                self.env["DOCTOR_TEST_NODE"] = version
                result = self.invoke()
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                if expected:
                    self.assertIn("MISSING  Node 22.6+", result.stdout)

    def test_missing_hooks_report_setup_instruction(self):
        self.env["DOCTOR_TEST_HOOKS"] = "1"
        result = self.invoke()
        self.assertEqual(result.returncode, 1)
        self.assertIn("MISSING  Git hooks — run: just setup", result.stdout)


if __name__ == "__main__":
    unittest.main()
