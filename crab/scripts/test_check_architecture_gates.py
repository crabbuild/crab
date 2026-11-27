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


class AuthScopeTests(unittest.TestCase):
    def test_arm_sdk_remains_optional_and_confined_to_the_auth_provider(self):
        required = ("crab-coordination", "crab-types", "serde", "serde_json", "thiserror", "tracing")
        azure = {"name": "azure_identity", "kind": None, "optional": True, "uses_default_features": False,
                 "features": GATES.WORKSPACE_PROVIDER_SDK_DEPENDENCIES["azure_identity"]["features"]}
        arm = {"azure-management": ["dep:azure_identity", "dep:reqwest"]}
        provider = "src/azure_management.rs"
        import_statement = "let credential = azure_identity::create_credential();"
        for file, line, dependency, features, expected in (
            (provider, import_statement, azure, arm, True),
            (provider, import_statement, azure | {"optional": False}, arm, False),
            (provider, import_statement, azure | {"kind": "dev"}, arm, False),
            (provider, import_statement, azure | {"features": ["enable_reqwest"]}, arm, False),
            (provider, import_statement, azure, arm | {"azure-entra-client": ["azure-management"]}, False),
            (provider, "use crab_storage::Store;", azure, arm, False),
            ("src/token_cache.rs", import_statement, azure, arm, False),
        ):
            with self.subTest(file=file, dependency=dependency, features=features), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                source = root / "crates/crab-auth" / file
                source.parent.mkdir(parents=True)
                source.write_text(line, encoding="utf-8")
                metadata = {"packages": [{"name": "crab-auth", "features": features, "dependencies": [
                    {"name": name, "kind": None, "optional": False, "features": []} for name in required
                ] + [dependency]}]}
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(GATES.check_auth_module_scope(root, metadata), expected)


class WorkspaceDependencyTests(unittest.TestCase):
    def workspace_policy(self, owner, dependencies, features=None):
        packages = [{"name": name, "dependencies": []} for name in GATES.WORKSPACE_DEPENDENCY_POLICY]
        package = next(package for package in packages if package["name"] == owner)
        package.update(dependencies=dependencies, features=features or {})
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return GATES.check_workspace_dependency_policy({"packages": packages})

    def test_storage_fixture_overlay_is_dev_only_on_an_existing_allowed_edge(self):
        native = {"name": "crab-storage", "kind": None, "features": [], "uses_default_features": False}
        fixture = native | {"kind": "dev", "features": ["test-support"]}
        cases = (
            ("crab-read", [native, fixture], {}, True),
            ("crab-read", [fixture], {}, False),
            ("crab-read", [native, fixture | {"kind": None}], {}, False),
            ("crab-read", [native, fixture | {"features": ["test-support", "provider-cdn-qualification"]}], {}, False),
            ("crab-read", [native, fixture | {"uses_default_features": True}], {}, False),
            ("crab-read", [native, fixture], {"qualification": ["crab-storage?/test-support"]}, False),
            ("crab-types", [native, fixture], {}, False),
        )
        for owner, dependencies, features, expected in cases:
            with self.subTest(owner=owner, dependencies=dependencies, features=features):
                self.assertEqual(self.workspace_policy(owner, dependencies, features), expected)

    def test_server_cdn_adapter_is_only_reachable_through_explicit_cdn_features(self):
        edge = {"name": "crab-auth-store", "kind": None, "optional": True,
                "uses_default_features": False, "features": []}
        features = {"provider-cdn": ["dep:crab-auth-store", "crab-auth-store/operator-cdn"],
                    "provider-cdn-qualification": ["provider-cdn", "crab-storage/provider-cdn-qualification"]}
        for owner in ("crab-server", "crab-s3-gateway"):
            with self.subTest(owner=owner):
                self.assertTrue(self.workspace_policy(owner, [edge], features))
                self.assertFalse(self.workspace_policy(owner, [edge | {"optional": False}], features))
                self.assertFalse(self.workspace_policy(owner, [edge | {"features": ["operator-cdn"]}], features))
                self.assertFalse(self.workspace_policy(owner, [edge], features | {"default": ["provider-cdn"]}))
                self.assertFalse(self.workspace_policy(owner, [edge], features | {"internal": ["provider-cdn-qualification"]}))
                self.assertFalse(self.workspace_policy(owner, [edge], features | {"internal": ["crab-auth-store?/operator-cdn"]}))
        self.assertFalse(self.workspace_policy("crab-read", [edge], features))

    def test_server_cdn_cache_stays_behind_the_provider_cdn_feature(self):
        edge = {"name": "crab-cache", "kind": None, "optional": True,
                "uses_default_features": False, "features": ["local-cache"]}
        features = {"provider-cdn": ["dep:crab-auth-store", "crab-auth-store/operator-cdn",
                                     "dep:crab-cache", "dep:crab-cache-store", "crab-cache-store/lfs"],
                    "provider-cdn-qualification": ["provider-cdn", "crab-storage/provider-cdn-qualification"]}
        self.assertTrue(self.workspace_policy("crab-server", [edge], features))
        self.assertFalse(self.workspace_policy("crab-server", [edge | {"optional": False}], features))
        self.assertFalse(self.workspace_policy("crab-server", [edge | {"features": []}], features))
        self.assertFalse(self.workspace_policy("crab-server", [edge], features | {"default": ["dep:crab-cache"]}))
        self.assertFalse(self.workspace_policy(
            "crab-server", [edge], features | {"provider-cdn": features["provider-cdn"] + ["dep:crab-vfs"]}))

    def test_package_dependencies_cannot_require_private_crates_even_when_optional(self):
        classified = (GATES.PRIVATE_INTERNAL_PACKAGES | GATES.PUBLISHED_LIBRARY_PACKAGES
                      | set(GATES.SHIPPED_BINARY_PACKAGES))
        for owner in ("crab-sdk", "crab-read"):
            for kind in (None, "build", "dev"):
                for optional in (False, True):
                    packages = [{
                        "name": name,
                        "version": "0.1.0",
                        "publish": [] if name in GATES.PRIVATE_INTERNAL_PACKAGES else None,
                        "targets": [{"name": binary, "kind": ["bin"]}
                                    for binary in GATES.SHIPPED_BINARY_PACKAGES.get(name, ())],
                        "dependencies": [],
                    } for name in classified]
                    package = next(package for package in packages if package["name"] == owner)
                    package["dependencies"] = [{
                        "name": "crab-vfs", "kind": kind, "optional": optional,
                    }]
                    with self.subTest(owner=owner, kind=kind, optional=optional):
                        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                            result = GATES.check_package_release_policy({"packages": packages})
                        self.assertEqual(result, kind == "dev")

    def test_cloud_transports_keep_rustls_and_system_certificate_roots(self):
        manifest = (GATES.repo_root() / "Cargo.toml").read_text(encoding="utf-8")
        cases = [("current transports", manifest, True)]
        for name in ("azure_identity", "azure_mgmt_storage", "azure_storage",
                     "azure_storage_blobs", "google-cloud-storage", "reqwest"):
            entry = next(line for line in manifest.splitlines() if line.startswith(name + " = "))
            declaration = ('{ version = "0.12", default-features = false, features = ["rustls-tls"] }'
                           if name == "reqwest" else '"0.24"' if name == "google-cloud-storage" else '"0.21"')
            cases.append((name, manifest.replace(entry, name + " = " + declaration), False))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, candidate, expected in cases:
                with self.subTest(transport=name):
                    (root / "Cargo.toml").write_text(candidate, encoding="utf-8")
                    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                        provider = GATES.check_workspace_provider_sdk_dependency_sources(root, {"packages": []})
                        shared = GATES.check_workspace_third_party_dependency_sources(root, {"packages": []})
                    self.assertEqual(provider and shared, expected)

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

    def test_results_composition_does_not_admit_product_dependencies(self):
        for owner, dependency, expected in (
            ("crab-sdk", "crab-workflow", True),
            ("crab-server", "crab-workflow", True),
            ("crab-sdk", "crab-server", False),
            ("crab-workflow", "crab-sdk", False),
        ):
            with self.subTest(owner=owner, dependency=dependency):
                packages = [
                    {"name": name, "dependencies": []}
                    for name in GATES.WORKSPACE_DEPENDENCY_POLICY
                ]
                package = next(package for package in packages if package["name"] == owner)
                package["dependencies"] = [{"name": dependency, "kind": None}]
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    result = GATES.check_workspace_dependency_policy({"packages": packages})
                self.assertEqual(result, expected)

    def test_results_primitives_do_not_admit_workflow_transport_policy(self):
        required = ("crab-types", "serde", "serde_json", "serde_yaml", "thiserror")
        for dependency, expected in (("base64", True), ("schemars", True), ("crab-sdk", False)):
            metadata = {"packages": [{
                "name": "crab-workflow",
                "dependencies": [
                    {"name": name, "kind": None, "optional": name == "schemars"}
                    for name in (*required, dependency)
                ],
            }]}
            with self.subTest(dependency=dependency), tempfile.TemporaryDirectory() as directory:
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    result = GATES.check_workflow_module_scope(Path(directory), metadata)
                self.assertEqual(result, expected)

    def test_sqlite_policy_requires_the_published_cellar_linkage(self):
        manifest = (GATES.repo_root() / "Cargo.toml").read_text(encoding="utf-8")
        entry = next(line for line in manifest.splitlines() if line.startswith("rusqlite = "))
        cases = (
            ('{ version = "=0.40.2", default-features = false, features = ["bundled", "cache", "fallible_uint"] }', True),
            ('{ version = "0.31", features = ["bundled"] }', False),
            ('{ version = "=0.40.2", default-features = false, features = ["bundled", "cache"] }', False),
            ('{ version = "=0.40.2", default-features = false, features = ["bundled", "fallible_uint"] }', False),
            ('{ version = "0.40.2", default-features = false, features = ["bundled", "cache", "fallible_uint"] }', False),
            ('{ version = "=0.40.2", features = ["bundled", "cache", "fallible_uint"] }', False),
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



class PackLayoutOwnershipTests(unittest.TestCase):
    def fixture(self, root):
        for name, patterns in GATES.STORAGE_PACK_LAYOUT_REQUIRED_DELEGATIONS.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("\n".join(patterns), encoding="utf-8")

    def check(self, root):
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return GATES.check_storage_pack_layout_delegation(root)

    def test_repack_requires_current_capsule_delegations(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            path = root / "crab/src/cmd/repack.rs"
            original = path.read_text(encoding="utf-8")
            self.assertTrue(self.check(root))
            for required in GATES.STORAGE_PACK_LAYOUT_REQUIRED_DELEGATIONS["crab/src/cmd/repack.rs"]:
                with self.subTest(required=required):
                    path.write_text(original.replace(required, ""), encoding="utf-8")
                    self.assertFalse(self.check(root))

    def test_pack_paths_and_manifest_repack_cannot_return(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            path = root / "crab/src/cmd/repack.rs"
            original = path.read_text(encoding="utf-8")
            forbidden = (*GATES.STORAGE_PACK_LAYOUT_FORBIDDEN_PATTERNS, *GATES.STORAGE_PACK_LAYOUT_FORBIDDEN_DELEGATIONS["crab/src/cmd/repack.rs"])
            for pattern in forbidden:
                with self.subTest(pattern=pattern):
                    path.write_text(original + "\n" + pattern, encoding="utf-8")
                    self.assertFalse(self.check(root))

    def test_push_policy_remains_scanned_without_a_retired_materializer(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            path = root / "crab/src/git/push.rs"
            self.assertTrue(self.check(root))
            forbidden = (
                *GATES.STORAGE_PACK_LAYOUT_FORBIDDEN_PATTERNS,
                *GATES.STORAGE_PACK_LAYOUT_FORBIDDEN_DELEGATIONS["crab/src/git/push.rs"],
            )
            for pattern in forbidden:
                with self.subTest(pattern=pattern):
                    path.write_text(pattern, encoding="utf-8")
                    self.assertFalse(self.check(root))

    def test_every_pack_layout_owner_is_required(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            for name in GATES.STORAGE_PACK_LAYOUT_REQUIRED_DELEGATIONS:
                with self.subTest(owner=name):
                    path = root / name
                    original = path.read_text(encoding="utf-8")
                    path.unlink()
                    self.assertFalse(self.check(root))
                    path.write_text(original, encoding="utf-8")



class ReadRetentionScopeTests(unittest.TestCase):
    def check_scope(self, source, *, optional=True, features=None, hydration=True):
        metadata = {"packages": [{
            "name": "crab-read",
            "features": {"hydration": ["dep:crab-coordination"] if hydration else []},
            "dependencies": [
                {"name": "object_store", "kind": None, "optional": False,
                 "uses_default_features": False, "features": []},
                {"name": "crab-coordination", "kind": None, "optional": optional,
                 "uses_default_features": False,
                 "features": ["object-store-lock"] if features is None else features},
            ],
        }]}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "crates/crab-read/src/hydrator.rs"
            path.parent.mkdir(parents=True)
            path.write_text(source)
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                return GATES.check_read_module_scope(root, metadata)

    def test_only_hydration_inherits_the_retention_runtime(self):
        self.assertTrue(self.check_scope(""))
        for options in ({"optional": False}, {"features": ["coordinator-dynamodb"]}, {"hydration": False}):
            with self.subTest(options=options):
                self.assertFalse(self.check_scope("", **options))

    def test_readers_pin_but_cannot_register_or_acquire_write_authority(self):
        for source, expected in (
            ("use crab_coordination::read_retention_runtime::RetainedRead;", True),
            ("use crab_coordination::read_retention_runtime::ReadRetentionPin;", True),
            ("source: crab_coordination::CoordinationError,", True),
            ("use crab_coordination::read_retention::ReadRetentionLease;", False),
            ("use crab_coordination::read_retention_runtime::ReadRetentionSession;", False),
            ("use crab_coordination::read_retention_runtime::*;", False),
            ("use crab_coordination::push_lock::PushLock;", False),
            ("use crab_coordination::{GcFenceLease};", False),
        ):
            with self.subTest(source=source):
                self.assertEqual(self.check_scope(source), expected)

    def test_test_claims_do_not_allow_product_credentials(self):
        self.assertTrue(self.check_scope("#[cfg(test)]\nmod tests {\nuse crab_coordination::read_retention::ReadRetentionLease;\n}"))
        self.assertFalse(self.check_scope("#[cfg(test)]\nmod tests {\nuse crab_auth::AuthConfig;\n}"))

if __name__ == "__main__":
    unittest.main()
