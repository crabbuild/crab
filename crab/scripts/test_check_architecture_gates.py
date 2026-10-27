"""Regression coverage for architecture scope boundaries."""

import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check-architecture-gates.py")
SPEC = importlib.util.spec_from_file_location("architecture_gates", SCRIPT)
GATES = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATES)


class CacheScopeTests(unittest.TestCase):
    def check_source(self, relative, text):
        metadata = {"packages": [{
            "name": "crab-cache",
            "dependencies": [
                {"name": name, "kind": None, "optional": False, "features": []}
                for name in ("crab-types", "crab-xet", "serde", "thiserror", "tracing")
            ],
        }]}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / relative
            source.parent.mkdir(parents=True)
            source.write_text(text, encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                return GATES.check_cache_module_scope(root, metadata)

    def test_admitted_fixture_lines_remain_path_and_line_scoped(self):
        for path, lines in GATES.CACHE_MODULE_TEST_LINES.items():
            for line in lines:
                with self.subTest(path=path, line=line):
                    self.assertTrue(self.check_source(path, line))
                    self.assertFalse(self.check_source("crates/crab-cache/src/policy.rs", line))
                    self.assertFalse(self.check_source(path, line + " use crab_storage::Store;"))

    def test_fixture_files_still_reject_unrelated_runtime_policy(self):
        for path in GATES.CACHE_MODULE_TEST_LINES:
            for line in ('println!("product output");', "use xet_data::FileReconstructor;", "use crab_auth::AuthConfig;"):
                with self.subTest(path=path, line=line):
                    self.assertFalse(self.check_source(path, line))

    def test_xet_adapter_remains_the_runtime_owner(self):
        line = "use xet_client::Client;"
        self.assertTrue(self.check_source("crates/crab-cache/src/xet_chunk_cache.rs", line))
        self.assertFalse(self.check_source("crates/crab-cache/src/catalog.rs", line))


class StorageScopeTests(unittest.TestCase):
    def test_dependency_prefixes_ignore_words_embedded_in_test_names(self):
        metadata = {"packages": [{
            "name": "crab-storage",
            "dependencies": [
                {"name": name, "kind": None, "optional": False,
                 "uses_default_features": False,
                 "features": ["aws", "gcp", "azure", "fs"] if name == "object_store" else []}
                for name in ("crab-types", "object_store", "thiserror", "tokio", "tracing")
            ],
        }]}
        cases = {
            "fn explicit_azure_identity_preserves_account_and_container() {}": True,
            "use azure_identity::DefaultAzureCredential;": False,
            "extern crate azure_identity;": False,
            "use aws_sdk_s3::Client;": False,
            "use crab::Storage;": False,
            "use clap::Parser;": False,
            'azure_identity = "0.21"': False,
            'println!("product output");': False,
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "crates/crab-storage/src/provider_store.rs"
            source.parent.mkdir(parents=True)
            for text, expected in cases.items():
                with self.subTest(source=text):
                    source.write_text(text, encoding="utf-8")
                    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                        result = GATES.check_storage_module_scope(root, metadata)
                    self.assertEqual(result, expected)


class WorkspaceDependencyTests(unittest.TestCase):
    def test_remote_fixture_dependency_does_not_admit_other_dependency_kinds(self):
        for name, kind, expected in (
            ("crab-storage", "dev", True),
            ("crab-storage", None, True),
            ("crab-cache", None, False),
            ("crab-auth", "dev", False),
        ):
            with self.subTest(dependency=name, kind=kind):
                packages = [
                    {"name": package, "dependencies": []}
                    for package in GATES.WORKSPACE_DEPENDENCY_POLICY
                ]
                remote = next(package for package in packages if package["name"] == "crab-remote")
                remote["dependencies"] = [{"name": name, "kind": kind}]
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    result = GATES.check_workspace_dependency_policy({"packages": packages})
                self.assertEqual(result, expected)

    def test_sqlite_policy_requires_the_published_cellar_linkage(self):
        manifest = (GATES.repo_root() / "Cargo.toml").read_text(encoding="utf-8")
        entry = next(line for line in manifest.splitlines() if line.startswith("rusqlite = "))
        cases = (
            ('{ version = "0.31", features = ["bundled"] }', True),
            ('{ version = "0.34", features = ["bundled"] }', False),
            ('{ version = "0.39", features = ["bundled", "fallible_uint"] }', False),
            ('{ version = "0.31", features = ["bundled", "blob"] }', False),
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for declaration, expected in cases:
                with self.subTest(declaration=declaration):
                    (root / "Cargo.toml").write_text(
                        manifest.replace(entry, "rusqlite = " + declaration), encoding="utf-8"
                    )
                    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                        result = GATES.check_workspace_third_party_dependency_sources(root, {"packages": []})
                    self.assertEqual(result, expected)


class CellarBoundaryTests(unittest.TestCase):
    def check_source(self, source: str, dependencies: tuple[str, ...] = (),
                     relative: str = "crates/crab-server/src/lib.rs") -> bool:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / relative
            path.parent.mkdir(parents=True)
            path.write_text(source, encoding="utf-8")
            metadata = {"packages": [{
                "name": "crab-server",
                "dependencies": [{"name": name} for name in dependencies],
            }]}
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                return GATES.check_cell_runtime_server_boundary(root, metadata)

    def test_cellar_import_is_admitted(self):
        self.assertTrue(self.check_source("use cellar::sdk::Node;\n"))

    def test_cellule_import_is_rejected(self):
        self.assertFalse(self.check_source("use cellule_runtime::CellRuntime;\n"))

    def test_native_construction_is_confined_to_the_runtime_module(self):
        source = "fn start() { Node::start_external(config); }\n"
        for relative, expected in [
            ("crates/crab-server/src/server/mod.rs", False),
            ("crates/crab-server/src/cells/runtime.rs", True),
        ]:
            with self.subTest(relative=relative):
                self.assertEqual(self.check_source(source, relative=relative), expected)

    def test_product_catalog_and_routing_stay_outside_native_ownership(self):
        self.assertTrue(self.check_source(
            "struct Server { catalog: CatalogStore, cell_capacity: Capacity, repository_cells: Router }\n",
            relative="crates/crab-server/src/server/mod.rs"))

    def test_retired_runtime_components_cannot_escape_into_server(self):
        self.assertFalse(self.check_source(
            "struct Server { catalog: CatalogStore, follower_store: FollowerStore }\n",
            relative="crates/crab-server/src/server/mod.rs"))

    def test_removed_workspace_dependency_is_rejected(self):
        self.assertFalse(self.check_source("", ("crab-cell-runtime",)))
        self.assertFalse(self.check_source("", ("crab-ltx",)))
        self.assertFalse(self.check_source("", ("cellule-runtime",)))

    def test_removed_import_is_rejected_even_in_tests(self):
        self.assertFalse(self.check_source("#[cfg(test)] mod tests { use crab_ltx::CellReplica; }\n"))


class PushOwnershipTests(unittest.TestCase):
    def check_sources(self, sources, checker):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for relative, text in sources.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text, encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                return checker(root)

    def test_head_policy_requires_root_authority_and_rejects_local_substitution(self):
        valid = "current_branch_is_denied(config, root.head(), &spec.dst)"
        sources = {path: valid for path in GATES.GIT_PUSH_HEAD_SCAN_PATHS}
        self.assertTrue(self.check_sources(sources, GATES.check_git_push_head_authority))
        for path in sources:
            with self.subTest(missing=path):
                missing = {key: value for key, value in sources.items() if key != path}
                self.assertFalse(self.check_sources(missing, GATES.check_git_push_head_authority))
            for pattern in GATES.GIT_PUSH_HEAD_FORBIDDEN_PATTERNS:
                with self.subTest(path=path, pattern=pattern):
                    changed = dict(sources, **{path: valid + "\n" + pattern})
                    self.assertFalse(self.check_sources(changed, GATES.check_git_push_head_authority))
        sources["crab/src/git/capsule_push.rs"] = ""
        self.assertFalse(self.check_sources(sources, GATES.check_git_push_head_authority))

    def test_tag_delegation_is_required_and_raw_peeling_rejected_on_all_push_owners(self):
        sources = {path: GATES.GIT_TAG_PEEL_REQUIRED_DELEGATIONS.get(path, "")
                   for path in GATES.GIT_TAG_PEEL_SCAN_PATHS}
        self.assertTrue(self.check_sources(sources, GATES.check_git_tag_peel_delegation))
        for path in sources:
            with self.subTest(missing=path):
                missing = {key: value for key, value in sources.items() if key != path}
                self.assertFalse(self.check_sources(missing, GATES.check_git_tag_peel_delegation))
            for pattern in GATES.GIT_TAG_PEEL_FORBIDDEN_PATTERNS:
                with self.subTest(path=path, pattern=pattern):
                    changed = dict(sources, **{path: sources[path] + "\n" + pattern})
                    self.assertFalse(self.check_sources(changed, GATES.check_git_tag_peel_delegation))
        for path in GATES.GIT_TAG_PEEL_REQUIRED_DELEGATIONS:
            with self.subTest(delegation=path):
                self.assertFalse(self.check_sources(dict(sources, **{path: ""}), GATES.check_git_tag_peel_delegation))


if __name__ == "__main__":
    unittest.main()
