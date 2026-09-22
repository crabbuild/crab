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


class CellRuntimeBoundaryTests(unittest.TestCase):
    def metadata(self, dependency_kind="dev"):
        return {
            "packages": [{
                "name": "crab-http-server",
                "dependencies": [{
                    "name": "cellule-ltx",
                    "kind": dependency_kind,
                    "optional": False,
                    "features": ["replica"],
                }],
            }],
        }

    def check_source(
        self,
        text,
        relative="crates/crab-http-server/src/lib.rs",
        dependency_kind="dev",
    ):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / relative
            source.parent.mkdir(parents=True)
            source.write_text(text, encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                return GATES.check_cell_runtime_server_boundary(
                    root,
                    self.metadata(dependency_kind),
                )

    def test_production_import_is_rejected(self):
        self.assertFalse(self.check_source("use cellule_ltx::CellReplica;\nfn route() {}\n"))

    def test_normal_and_build_dependencies_are_rejected(self):
        for dependency_kind in (None, "build"):
            with self.subTest(dependency_kind=dependency_kind):
                self.assertFalse(
                    self.check_source("fn route() {}\n", dependency_kind=dependency_kind)
                )

    def test_cfg_test_module_and_nested_test_module_are_admitted(self):
        source = """fn route() {}

#[cfg(test)]
mod tests {
    mod nested {
        use cellule_ltx::CellReplica;
    }
}

fn later_production_code() {}
"""
        self.assertTrue(self.check_source(source))

    def test_test_file_is_admitted_but_production_after_cfg_block_is_not(self):
        self.assertTrue(
            self.check_source(
                "use cellule_ltx::CellReplica;\n",
                relative="crates/crab-http-server/src/cells/scheduler/tests.rs",
            )
        )

    def test_production_server_component_fields_are_rejected(self):
        for field in (
            "cell_runtime",
            "catalog",
            "scheduler_status",
            "cell_capacity",
            "repository_cells",
            "peer_receiver",
            "follower_store",
            "node_log_transport",
        ):
            with self.subTest(field=field):
                self.assertFalse(
                    self.check_source(
                        "pub(crate) struct Server {\n"
                        f"    {field}: usize,\n"
                        "}\n",
                        relative="crates/crab-http-server/src/server.rs",
                    )
                )

    def test_test_only_server_component_fields_are_admitted(self):
        self.assertTrue(
            self.check_source(
                "pub(crate) struct Server {\n"
                "    #[cfg(test)]\n"
                "    repository_cells: usize,\n"
                "}\n",
                relative="crates/crab-http-server/src/server.rs",
            )
        )
        self.assertFalse(
            self.check_source(
                "#[cfg(test)]\nmod tests { use cellule_ltx::CellReplica; }\n"
                "use cellule_ltx::Db;\n",
            )
        )


if __name__ == "__main__":
    unittest.main()
