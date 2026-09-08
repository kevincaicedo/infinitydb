#!/usr/bin/env python3
"""Run the real inventory and DAG gates on isolated, deliberately invalid trees."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parent


class GateFixtures(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="inf-gate-selftest-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        (self.root / "scripts").mkdir()
        (self.root / "crates/fake/src").mkdir(parents=True)
        (self.root / "bins").mkdir()
        self.write("crates/fake/src/lib.rs", "pub fn safe() {}\n")
        for name in ["check-safety-inventory.sh", "check-dep-dag.sh"]:
            source = (SCRIPTS / name).read_text()
            # Confine the pre-fix gate's shared path to this fixture.
            source = source.replace("/tmp/inf-metadata.json", str(self.root / "legacy-metadata.json"))
            self.write(f"scripts/{name}", source)
        self.env = dict(os.environ, INF_CHECK_ROOT=str(self.root))

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def gate(self, name):
        return subprocess.run(
            ["bash", str(self.root / "scripts" / name)], cwd=self.root,
            env=self.env, text=True, capture_output=True, timeout=15,
        )

    def inventory(self, source, entry):
        self.write(f"crates/fake/src/{source}", "unsafe fn fixture() {}\n")
        self.write("crates/fake/SAFETY.md", f"# Inventory\n\n`{entry}`\n")
        return self.gate("check-safety-inventory.sh")

    def assert_red(self, result):
        self.assertNotEqual(result.returncode, 0, f"false green: {result.stdout}{result.stderr}")

    def assert_green(self, result):
        self.assertEqual(result.returncode, 0, f"false red: {result.stdout}{result.stderr}")

    def test_inventory_substring_is_not_a_file(self):
        self.assert_red(self.inventory("bytes.rs", "log_bytes.rs"))

    def test_inventory_dot_is_literal(self):
        self.assert_red(self.inventory("net.rs", "netXrs"))

    def test_inventory_extension_is_exact(self):
        self.assert_red(self.inventory("net.rs", "net.rs.bak"))

    def test_inventory_nested_basename_is_not_a_path(self):
        self.assert_red(self.inventory("a/bytes.rs", "b/bytes.rs"))

    def test_inventory_nested_file_needs_its_directory(self):
        self.assert_red(self.inventory("a/bytes.rs", "bytes.rs"))

    def test_inventory_accepts_exact_root_name(self):
        self.assert_green(self.inventory("bytes.rs", "bytes.rs::fixture"))

    def test_inventory_accepts_exact_nested_path(self):
        self.assert_green(self.inventory("a/bytes.rs", "a/bytes.rs::fixture"))

    def test_inventory_accepts_src_path(self):
        self.assert_green(self.inventory("a/bytes.rs", "src/a/bytes.rs"))

    def test_inventory_accepts_workspace_path(self):
        self.assert_green(self.inventory("a/bytes.rs", "crates/fake/src/a/bytes.rs"))

    def test_inventory_missing_inventory_fails(self):
        self.write("crates/fake/src/unsafe.rs", "unsafe fn fixture() {}\n")
        self.assert_red(self.gate("check-safety-inventory.sh"))

    def test_inventory_empty_scope_fails(self):
        shutil.rmtree(self.root / "crates/fake")
        self.assert_red(self.gate("check-safety-inventory.sh"))

    def test_inventory_missing_source_fails(self):
        shutil.rmtree(self.root / "crates/fake/src")
        self.assert_red(self.gate("check-safety-inventory.sh"))

    def test_inventory_safe_tree_reports_scope(self):
        result = self.gate("check-safety-inventory.sh")
        self.assert_green(result)
        self.assertIn("1 crates, 1 Rust files, 0 unsafe-bearing files", result.stdout)

    def metadata(self, kind=None):
        return {"packages": [
            {"name": "fake", "id": "fake", "dependencies": [
                {"name": "other", "kind": kind, "rename": "alias", "target": "cfg(unix)"},
            ]},
            {"name": "other", "id": "other", "dependencies": []},
        ], "workspace_members": ["fake", "other"]}

    def prepare_dag(self, kind=None, allowed=False):
        edge = '["other"]' if allowed else '[]'
        self.write("docs/dep-dag.toml", f"[edges]\nfake = {edge}\nother = []\n")
        self.write("metadata.json", json.dumps(self.metadata(kind)))
        cargo = self.write("tools/cargo", """#!/usr/bin/env python3
import os
from pathlib import Path
import sys
root = Path(os.environ['GATE_TEST_ROOT'])
if os.environ.get('GATE_TEST_CARGO_FAIL'):
    sys.exit(17)
sys.stdout.write((root / 'metadata.json').read_text())
sys.stdout.flush()
overwrite = root / 'overwrite.json'
if overwrite.exists():
    (root / 'legacy-metadata.json').write_text(overwrite.read_text())
""")
        cargo.chmod(0o700)
        self.env.update(GATE_TEST_ROOT=str(self.root), PATH=f"{cargo.parent}:{os.environ['PATH']}")

    def test_dag_forbidden_normal_edge(self):
        self.prepare_dag()
        result = self.gate("check-dep-dag.sh")
        self.assert_red(result)
        self.assertIn("fake -> other", result.stdout)

    def test_dag_forbidden_build_edge(self):
        self.prepare_dag(kind="build")
        self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_allowed_normal_edge(self):
        self.prepare_dag(allowed=True)
        self.assert_green(self.gate("check-dep-dag.sh"))

    def test_dag_dev_exemption_is_visible(self):
        self.prepare_dag(kind="dev")
        result = self.gate("check-dep-dag.sh")
        self.assert_green(result)
        self.assertIn("dev-edge exempt: fake -> other", result.stdout)

    def test_dag_shared_metadata_overwrite_cannot_hide_an_edge(self):
        self.prepare_dag()
        clean = self.metadata()
        clean["packages"][0]["dependencies"] = []
        self.write("overwrite.json", json.dumps(clean))
        result = self.gate("check-dep-dag.sh")
        self.assert_red(result)
        self.assertIn("fake -> other", result.stdout)

    def test_dag_never_writes_legacy_metadata_path(self):
        self.prepare_dag(allowed=True)
        legacy = self.write("legacy-metadata.json", "fixture-owned sentinel")
        self.assert_green(self.gate("check-dep-dag.sh"))
        self.assertEqual(legacy.read_text(), "fixture-owned sentinel")

    def test_dag_cargo_failure_fails(self):
        self.prepare_dag()
        self.env["GATE_TEST_CARGO_FAIL"] = "1"
        self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_bad_metadata_fails(self):
        self.prepare_dag()
        self.write("metadata.json", "not json")
        self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_empty_scope_fails(self):
        self.prepare_dag()
        self.write("metadata.json", '{"packages": [], "workspace_members": []}')
        self.assert_red(self.gate("check-dep-dag.sh"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
