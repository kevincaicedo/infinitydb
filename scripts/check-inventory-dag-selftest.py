#!/usr/bin/env python3
"""Plant invalid trees in the inventory, dependency and document gates."""

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

    def reserve_edge(self, reason="ADR-0106 D16: M7 stub activation"):
        with (self.root / "docs/dep-dag.toml").open("a") as policy:
            policy.write(f'\n[unused.fake]\nother = {json.dumps(reason)}\n')

    def remove_actual_edge(self):
        metadata = self.metadata()
        metadata["packages"][0]["dependencies"] = []
        self.write("metadata.json", json.dumps(metadata))

    def test_dag_unused_permission_needs_a_reservation(self):
        self.prepare_dag(allowed=True)
        self.remove_actual_edge()
        result = self.gate("check-dep-dag.sh")
        self.assert_red(result)
        self.assertIn("fake -> other", result.stdout)

    def test_dag_dev_edge_does_not_satisfy_normal_permission(self):
        self.prepare_dag(kind="dev", allowed=True)
        self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_reserved_dev_only_permission_is_visible(self):
        self.prepare_dag(kind="dev", allowed=True)
        self.reserve_edge()
        result = self.gate("check-dep-dag.sh")
        self.assert_green(result)
        self.assertIn("reserved edge: fake -> other", result.stdout)
        self.assertIn("dev-edge exempt: fake -> other", result.stdout)

    def test_dag_activation_requires_retiring_annotation(self):
        self.prepare_dag(allowed=True)
        self.reserve_edge()
        self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_annotation_cannot_authorize_forbidden_edge(self):
        self.prepare_dag()
        self.remove_actual_edge()
        self.reserve_edge()
        self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_reservation_needs_reason_decision_and_milestone(self):
        self.prepare_dag(allowed=True)
        self.remove_actual_edge()
        for reason in ["", "later", "ADR-0106 later", "M7 later"]:
            with self.subTest(reason=reason):
                self.write("docs/dep-dag.toml", '[edges]\nfake = ["other"]\nother = []\n')
                self.reserve_edge(reason)
                self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_missing_row_fails_even_without_dependencies(self):
        self.prepare_dag()
        self.remove_actual_edge()
        self.write("docs/dep-dag.toml", "[edges]\nfake = []\n")
        self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_unknown_package_row_fails(self):
        self.prepare_dag(allowed=True)
        with (self.root / "docs/dep-dag.toml").open("a") as policy:
            policy.write("retired = []\n")
        self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_unknown_target_fails(self):
        self.prepare_dag(allowed=True)
        self.write("docs/dep-dag.toml", '[edges]\nfake = ["other", "typo"]\nother = []\n')
        self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_duplicate_permission_fails(self):
        self.prepare_dag(allowed=True)
        self.write("docs/dep-dag.toml", '[edges]\nfake = ["other", "other"]\nother = []\n')
        self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_schema_does_not_accept_substring_membership(self):
        self.prepare_dag(allowed=True)
        self.write("docs/dep-dag.toml", '[edges]\nfake = "other-extra"\nother = []\n')
        self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_unknown_policy_section_fails(self):
        self.prepare_dag(allowed=True)
        with (self.root / "docs/dep-dag.toml").open("a") as policy:
            policy.write("\n[unusd.fake]\nother = 'ADR-0106 M7 stub'\n")
        self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_unknown_empty_reservation_package_fails(self):
        self.prepare_dag(allowed=True)
        with (self.root / "docs/dep-dag.toml").open("a") as policy:
            policy.write("\n[unused.typo]\n")
        self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_incomplete_workspace_metadata_fails(self):
        self.prepare_dag(allowed=True)
        metadata = self.metadata()
        metadata["workspace_members"].append("missing")
        self.write("metadata.json", json.dumps(metadata))
        result = self.gate("check-dep-dag.sh")
        self.assert_red(result)
        self.assertNotIn("Traceback", result.stderr)

    def test_dag_wrong_metadata_shape_is_named(self):
        self.prepare_dag()
        self.write("metadata.json", '{"packages": {}}')
        result = self.gate("check-dep-dag.sh")
        self.assert_red(result)
        self.assertIn("SCOPE ERROR", result.stderr)
        self.assertNotIn("Traceback", result.stderr)

    def test_dag_optional_target_edge_counts_before_feature_activation(self):
        self.prepare_dag(allowed=True)
        metadata = self.metadata()
        metadata["packages"][0]["dependencies"][0]["optional"] = True
        self.write("metadata.json", json.dumps(metadata))
        self.assert_green(self.gate("check-dep-dag.sh"))

    def test_dag_normal_and_build_declarations_count_as_one_edge(self):
        self.prepare_dag(allowed=True)
        metadata = self.metadata()
        metadata["packages"][0]["dependencies"].append(
            {"name": "other", "kind": "build", "target": None}
        )
        self.write("metadata.json", json.dumps(metadata))
        self.assert_green(self.gate("check-dep-dag.sh"))

    def test_dag_zero_dependency_policy_covers_external_packages(self):
        self.prepare_dag()
        self.write("docs/dep-dag.toml", 'zero-dependency = ["fake"]\n[edges]\nfake = []\nother = []\n')
        for kind in [None, "build", "dev"]:
            with self.subTest(kind=kind):
                metadata = self.metadata(kind)
                metadata["packages"][0]["dependencies"][0]["name"] = "external"
                self.write("metadata.json", json.dumps(metadata))
                self.assert_red(self.gate("check-dep-dag.sh"))

    def test_dag_zero_dependency_policy_accepts_empty_package(self):
        self.prepare_dag()
        self.remove_actual_edge()
        self.write("docs/dep-dag.toml", 'zero-dependency = ["fake"]\n[edges]\nfake = []\nother = []\n')
        self.assert_green(self.gate("check-dep-dag.sh"))


class DocumentPaths(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="inf-doc-paths-")
        self.addCleanup(self.scratch.cleanup)
        self.parent = Path(self.scratch.name)
        self.root = self.parent / "infinitydb"
        (self.root / "docs").mkdir(parents=True)
        (self.root / "Cargo.toml").write_text("[workspace]\n")
        (self.root / "ARCHITECTURE.md").write_text("# Architecture\n")
        (self.root / "docs/INFINITY_STYLE.md").write_text("# Style\n")
        (self.root / "docs/compat-matrix.md").write_text("**GENERATED — do not edit.**\n")

    def governance(self):
        docs = self.parent / "docs"
        (docs / "adr").mkdir(parents=True)
        (docs / "milestones").mkdir()
        (docs / "infinity-master-plan.md").write_text("# Master\n")
        (docs / "milestones/m0.md").write_text("# M0\n")
        (docs / "adr/0106-gates.md").write_text("# ADR-0106: Gates\n")
        (docs / "compat-matrix.md").write_text(
            "# Compatibility matrix\n\n"
            "The generated [compatibility matrix](../infinitydb/docs/compat-matrix.md) "
            "lives in the Rust workspace.\n"
        )
        return docs

    def gate(self):
        return subprocess.run(
            ["bash", str(SCRIPTS / "check-doc-artifacts.sh")],
            env=dict(os.environ, INF_CHECK_ROOT=str(self.root)),
            text=True, capture_output=True, timeout=15,
        )

    def assert_red(self):
        result = self.gate()
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_doc_valid_workspace_and_parent_links(self):
        self.governance()
        (self.root / "ARCHITECTURE.md").write_text(
            "[local](docs/INFINITY_STYLE.md#safety) [parent](../docs/infinity-master-plan.md)\n"
        )
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_doc_missing_local_link_in_standalone_is_red(self):
        (self.root / "ARCHITECTURE.md").write_text("[wrong](docs/missing.md)\n")
        self.assert_red()

    def test_doc_titled_wrapped_and_reference_links_are_checked(self):
        for link in ['[wrong](docs/missing.md "title")', '[wrong](<docs/missing.md>)',
                     '[wrong]: docs/missing.md "title"']:
            with self.subTest(link=link):
                (self.root / "ARCHITECTURE.md").write_text(link + "\n")
                self.assert_red()

    def test_doc_standalone_parent_link_is_disclosed(self):
        (self.root / "ARCHITECTURE.md").write_text("[parent](../docs/infinity-master-plan.md)\n")
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("1 parent links unvalidated", result.stdout)

    def test_doc_parent_links_are_checked_with_governance(self):
        self.governance()
        (self.root / "ARCHITECTURE.md").write_text("[wrong](../docs/missing.md)\n")
        self.assert_red()

    def test_doc_every_milestone_link_is_checked(self):
        docs = self.governance()
        (docs / "milestones/new.md").write_text("[wrong](missing.md)\n")
        self.assert_red()

    def test_doc_missing_governing_workspace_input_is_red(self):
        (self.root / "ARCHITECTURE.md").unlink()
        self.assert_red()

    def test_doc_empty_milestone_scope_is_red(self):
        docs = self.governance()
        (docs / "milestones/m0.md").unlink()
        self.assert_red()

    def test_doc_missing_milestone_directory_is_red(self):
        docs = self.governance()
        (docs / "milestones/m0.md").unlink()
        (docs / "milestones").rmdir()
        self.assert_red()

    def test_doc_obsolete_layout_root_is_red(self):
        docs = self.governance()
        (docs / "infinity-master-plan.md").write_text("```text\ninfinity/\n```\n")
        self.assert_red()

    def test_doc_obsolete_compat_path_is_red(self):
        docs = self.governance()
        (docs / "infinity-master-plan.md").write_text("`tests/compat-suite`\n")
        self.assert_red()

    def test_doc_landed_adr_placeholder_is_red(self):
        docs = self.governance()
        (docs / "milestones/m0.md").write_text("`docs/adr/00xx-log-io-tier.md`\n")
        self.assert_red()

    def test_doc_numbered_adr_citation_must_resolve(self):
        docs = self.governance()
        (docs / "milestones/m0.md").write_text("`docs/adr/0106-wrong-name.md`\n")
        self.assert_red()

    def test_doc_historical_legacy_path_is_not_a_current_link(self):
        docs = self.governance()
        (docs / "infinity-master-plan.md").write_text("`docs/vortex-master-plan.md`\n")
        self.assert_red()


if __name__ == "__main__":
    unittest.main(verbosity=2)
