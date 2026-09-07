import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("prepare_release.py")


class PreparationTests(unittest.TestCase):
    def test_updates_package_and_lock_versions_while_preserving_dependencies(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = (
                '[package]\nname = "hooklinesinker"\nversion = "1.0.0"\nedition = "2024"\n'
                '\n[dependencies]\nhelper = { path = "helper" }\n'
                '\n[package.metadata]\nversion = "unchanged"\n'
            )
            (root / "Cargo.toml").write_text(manifest)
            (root / "src").mkdir()
            (root / "src/lib.rs").write_text("")
            helper = root / "helper"
            (helper / "src").mkdir(parents=True)
            (helper / "src/lib.rs").write_text("")
            (helper / "Cargo.toml").write_text(
                '[package]\nname = "helper"\nversion = "2.3.4"\nedition = "2024"\n'
            )
            subprocess.run(
                ["cargo", "metadata", "--offline", "--format-version", "1"],
                cwd=root,
                capture_output=True,
                check=True,
            )
            original_lock = (root / "Cargo.lock").read_text()
            for version in ["1.0.0", "9.8.7"]:
                with self.subTest(version=version):
                    subprocess.run(
                        [sys.executable, str(SCRIPT), version],
                        cwd=root,
                        capture_output=True,
                        check=True,
                    )
                    self.assertEqual(
                        (root / "Cargo.toml").read_text(),
                        manifest.replace('version = "1.0.0"', f'version = "{version}"'),
                    )
                    self.assertEqual(
                        (root / "Cargo.lock").read_text(),
                        original_lock.replace('version = "1.0.0"', f'version = "{version}"'),
                    )
                    metadata = json.loads(
                        subprocess.check_output(
                            ["cargo", "metadata", "--offline", "--locked", "--format-version", "1"],
                            cwd=root,
                            text=True,
                        )
                    )
                    self.assertEqual(
                        {package["name"]: package["version"] for package in metadata["packages"]},
                        {"hooklinesinker": version, "helper": "2.3.4"},
                    )

    def test_missing_package_version_preserves_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "Cargo.toml"
            original = (
                '[package]\nname = "hooklinesinker"\n\n[package.metadata]\nversion = "keep"\n'
            )
            path.write_text(original)
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "9.8.7"],
                cwd=directory,
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Expected one version", result.stderr)
            self.assertEqual(path.read_text(), original)


if __name__ == "__main__":
    unittest.main()
