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
    def test_object_store_feature_ownership_distinguishes_test_fixtures(self):
        metadata = {"packages": [{
            "name": "crab-ltx",
            "dependencies": [
                {
                    "name": "object_store", "kind": None,
                    "uses_default_features": False, "features": [],
                },
                {
                    "name": "object_store", "kind": "dev",
                    "uses_default_features": False, "features": ["fs"],
                },
            ],
        }]}
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertTrue(GATES.check_object_store_features(metadata))
            metadata["packages"][0]["dependencies"][0]["features"] = ["fs"]
            self.assertFalse(GATES.check_object_store_features(metadata))

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
                    "name": "crab-ltx",
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
        self.assertFalse(self.check_source("use crab_ltx::CellReplica;\nfn route() {}\n"))

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
        use crab_ltx::CellReplica;
    }
}

fn later_production_code() {}
"""
        self.assertTrue(self.check_source(source))

    def test_test_file_is_admitted_but_production_after_cfg_block_is_not(self):
        self.assertTrue(
            self.check_source(
                "use crab_ltx::CellReplica;\n",
                relative="crates/crab-http-server/src/cells/scheduler/tests.rs",
            )
        )
        self.assertFalse(
            self.check_source(
                "#[cfg(test)]\nmod tests { use crab_ltx::CellReplica; }\n"
                "use crab_ltx::ManagedDb;\n",
            )
        )


class CellCoordinationKernelTests(unittest.TestCase):
    def check_kernel(self, kernel, actor):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel_path = root / GATES.CELL_RUNTIME_COORDINATION_KERNEL_PATH
            actor_path = root / GATES.CELL_RUNTIME_COORDINATION_ACTOR_PATH
            kernel_path.parent.mkdir(parents=True, exist_ok=True)
            actor_path.parent.mkdir(parents=True, exist_ok=True)
            kernel_path.write_text(kernel, encoding="utf-8")
            actor_path.write_text(actor, encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                return GATES.check_cell_runtime_coordination_kernel(root)

    def test_pure_kernel_and_actor_adapter_are_admitted(self):
        kernel = """pub(crate) enum CoordinationInput {}
pub(crate) enum CoordinationDecision {}
pub(crate) struct CoordinationState;
impl CoordinationState {
    pub(crate) fn step(&mut self, input: CoordinationInput) {}
}
"""
        self.assertTrue(
            self.check_kernel(
                kernel,
                "use crate::coordination::CoordinationState;\n"
                "active.coordination.step(CoordinationInput::Fence);\n",
            )
        )

    def test_kernel_rejects_async_and_provider_adapters(self):
        kernel = """pub(crate) enum CoordinationInput {}
pub(crate) enum CoordinationDecision {}
pub(crate) struct CoordinationState;
impl CoordinationState {
    pub(crate) fn step(&mut self, input: CoordinationInput) {}
    async fn read() { object_store::get().await; }
}
"""
        self.assertFalse(
            self.check_kernel(
                kernel,
                "use crate::coordination::CoordinationState;\n"
                "active.coordination.step(CoordinationInput::Fence);\n",
            )
        )

    def test_actor_must_retain_the_kernel_adapter_call(self):
        kernel = """pub(crate) enum CoordinationInput {}
pub(crate) enum CoordinationDecision {}
pub(crate) struct CoordinationState;
impl CoordinationState {
    pub(crate) fn step(&mut self, input: CoordinationInput) {}
}
"""
        self.assertFalse(self.check_kernel(kernel, "fn actor() {}\n"))


class StandaloneLtxHardCutTests(unittest.TestCase):
    def check_source(self, text):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "crates/crab-ltx/src/lib.rs"
            source.parent.mkdir(parents=True)
            source.write_text(text, encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                return GATES.check_standalone_ltx_hard_cut(root)

    def test_retired_epoch_head_symbols_are_rejected(self):
        for symbol in (
            "Replica",
            "ReplicaHead",
            "PagedDatabase",
            "PagedConnection",
            "CompactionSchedule",
        ):
            with self.subTest(symbol=symbol):
                self.assertFalse(self.check_source(f"pub struct {symbol};\n"))

    def test_cell_scoped_surfaces_are_admitted(self):
        self.assertTrue(
            self.check_source(
                "pub struct CellReplica;\n"
                "pub struct CellPagedDatabase;\n"
                "pub struct CellWritableDatabase;\n"
            )
        )

    def test_canonical_replica_module_is_admitted_but_storage_markers_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "crates/crab-ltx/src/replica.rs"
            source.parent.mkdir(parents=True)
            source.write_text("pub struct CellReplica;\n", encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
                io.StringIO()
            ):
                self.assertTrue(GATES.check_standalone_ltx_hard_cut(root))

        self.assertFalse(self.check_source("const PREFIX: &str = \"ltx/<epoch>\";\n"))
        self.assertFalse(self.check_source("const HEAD: &str = \"head.json\";\n"))
        self.assertFalse(self.check_source("const MANIFEST: &str = \"manifest.json\";\n"))

    def test_retired_directories_and_examples_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paged_map = root / "crates/crab-ltx/src/paged/map.rs"
            paged_map.parent.mkdir(parents=True)
            paged_map.write_text("pub fn page_map() {}\n", encoding="utf-8")
            example = root / "crates/crab-ltx/examples/replica_roundtrip.rs"
            example.parent.mkdir(parents=True)
            example.write_text("fn main() {}\n", encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
                io.StringIO()
            ):
                self.assertFalse(GATES.check_standalone_ltx_hard_cut(root))


if __name__ == "__main__":
    unittest.main()
