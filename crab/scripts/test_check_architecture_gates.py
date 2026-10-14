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


class CelluleBoundaryTests(unittest.TestCase):
    def check_source(self, source: str, dependencies: tuple[str, ...] = ()) -> bool:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "crates/crab-http-server/src/lib.rs"
            path.parent.mkdir(parents=True)
            path.write_text(source, encoding="utf-8")
            metadata = {"packages": [{
                "name": "crab-http-server",
                "dependencies": [{"name": name} for name in dependencies],
            }]}
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                return GATES.check_cell_runtime_server_boundary(root, metadata)

    def test_cellule_import_is_admitted(self):
        self.assertTrue(self.check_source("use cellule_runtime::CellRuntime;\n"))

    def test_removed_workspace_dependency_is_rejected(self):
        self.assertFalse(self.check_source("", ("crab-cell-runtime",)))
        self.assertFalse(self.check_source("", ("crab-ltx",)))

    def test_removed_import_is_rejected_even_in_tests(self):
        self.assertFalse(self.check_source("#[cfg(test)] mod tests { use crab_ltx::CellReplica; }\n"))


class PackageReleasePolicyTests(unittest.TestCase):
    def metadata(self):
        packages = []
        for name in sorted(GATES.PRIVATE_INTERNAL_PACKAGES | GATES.PUBLISHED_LIBRARY_PACKAGES | set(GATES.SHIPPED_BINARY_PACKAGES)):
            packages.append({
                "name": name,
                "version": "1.2.4" if name in GATES.SHIPPED_BINARY_PACKAGES else "0.2.0" if name == "crab-remote-git" else "0.1.0",
                "publish": [] if name in GATES.PRIVATE_INTERNAL_PACKAGES else None,
                "dependencies": [{"name": "crab-remote-git", "req": "=0.2.0"}] if name == "crab" else [],
                "targets": [{"name": binary, "kind": ["bin"]} for binary in GATES.SHIPPED_BINARY_PACKAGES.get(name, set())],
            })
        return {"packages": packages}

    def check(self, metadata):
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return GATES.check_package_release_policy(metadata)

    def test_library_api_versions_are_independent_of_product_versions(self):
        self.assertTrue(self.check(self.metadata()))

    def test_library_consumers_must_pin_the_declared_api_version(self):
        for requirement in ("=0.1.0", "^0.2.0"):
            with self.subTest(requirement=requirement):
                metadata = self.metadata()
                package = GATES.package_by_name(metadata, "crab")
                package["dependencies"][0]["req"] = requirement
                self.assertFalse(self.check(metadata))

    def test_library_publishability_and_private_release_policy_remain_enforced(self):
        for name, field, value in (("crab-remote-git", "publish", []), ("crab-http-server", "version", "0.2.0"), ("crab-http-server", "publish", None), ("crab-auth-server", "version", "1.2.3")):
            with self.subTest(name=name, field=field):
                metadata = self.metadata()
                GATES.package_by_name(metadata, name)[field] = value
                self.assertFalse(self.check(metadata))


if __name__ == "__main__":
    unittest.main()
