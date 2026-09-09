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


if __name__ == "__main__":
    unittest.main()
