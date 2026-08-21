"""Tests for the RBAC policy engine."""

import pytest

from src.policy import PolicyEngine


class TestPolicyEvaluation:
    """Test the policy engine's rule matching logic."""

    def setup_method(self):
        from tests.conftest import SAMPLE_POLICY

        self.engine = PolicyEngine.from_dict(SAMPLE_POLICY)

    def test_admin_group_has_full_access(self):
        decision = self.engine.evaluate(
            identity="admin@corp.example.com",
            groups=["platform-admins"],
            repo_url="crab://bucket/any/repo",
            operation="push",
        )
        assert decision.allowed
        assert "write" in decision.permissions

    def test_protected_repo_converts_write_to_immutable_write(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "protected_repos": ["restricted/*"],
            "rules": [
                {
                    "group": "platform-admins",
                    "repos": ["*"],
                    "operations": ["push"],
                },
            ],
        })
        decision = engine.evaluate(
            identity="admin@corp.example.com",
            groups=["platform-admins"],
            repo_url="crab://bucket/restricted/repo",
            operation="push",
        )
        assert decision.allowed
        assert decision.protected_repo
        assert decision.permissions == ["read", "immutable-write"]

    def test_s3_provider_is_supported(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "s3",
            "rules": [
                {
                    "group": "ml-team",
                    "repos": ["models/*"],
                    "operations": ["clone"],
                },
            ],
        })

        decision = engine.evaluate(
            identity="alice@corp.example.com",
            groups=["ml-team"],
            repo_url="crab://bucket/models/gpt4",
            operation="clone",
        )

        assert decision.allowed
        assert decision.provider == "s3"

    def test_path_acl_requires_all_changed_paths_to_match_allow_rule(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "ml-team",
                    "repos": ["models/*"],
                    "operations": ["push"],
                    "write_paths": ["models/**", "README.md"],
                },
            ],
        })
        decision = engine.evaluate(
            identity="bob@corp.example.com",
            groups=["ml-team"],
            repo_url="crab://bucket/models/gpt4",
            operation="push",
            changed_paths=["models/tokenizer.json", "README.md"],
        )
        assert decision.allowed

        decision = engine.evaluate(
            identity="bob@corp.example.com",
            groups=["ml-team"],
            repo_url="crab://bucket/models/gpt4",
            operation="push",
            changed_paths=["models/tokenizer.json", "infra/prod.tf"],
        )
        assert not decision.allowed

    def test_write_path_acl_unions_matching_allow_rules(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "ml-team",
                    "repos": ["models/*"],
                    "operations": ["push"],
                    "write_paths": ["models/**"],
                },
                {
                    "identity": "bob@corp.example.com",
                    "repos": ["models/*"],
                    "operations": ["push"],
                    "write_paths": ["README.md"],
                },
            ],
        })

        decision = engine.evaluate(
            identity="bob@corp.example.com",
            groups=["ml-team"],
            repo_url="crab://bucket/models/gpt4",
            operation="push",
            changed_paths=["models/tokenizer.json", "README.md"],
        )

        assert decision.allowed

    def test_path_acl_requires_non_empty_changed_path_set(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "ml-team",
                    "repos": ["models/*"],
                    "operations": ["push"],
                    "write_paths": ["models/**"],
                },
            ],
        })

        decision = engine.evaluate(
            identity="bob@corp.example.com",
            groups=["ml-team"],
            repo_url="crab://bucket/models/gpt4",
            operation="push",
            changed_paths=[],
        )

        assert not decision.allowed

    def test_read_path_acl_returns_effective_scope(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "ml-team",
                    "repos": ["models/*"],
                    "operations": ["clone"],
                    "read_paths": ["src/**"],
                },
                {
                    "identity": "alice@corp.example.com",
                    "repos": ["models/*"],
                    "operations": ["clone"],
                    "read_paths": ["README.md"],
                },
            ],
            "deny": [
                {
                    "identity": "*",
                    "repos": ["models/*"],
                    "operations": ["clone"],
                    "read_paths": ["src/secrets/**"],
                },
            ],
        })

        decision = engine.evaluate(
            identity="alice@corp.example.com",
            groups=["ml-team"],
            repo_url="crab://bucket/models/gpt4",
            operation="clone",
        )

        assert decision.allowed
        assert decision.read_paths == ["src/**", "README.md"]
        assert decision.denied_read_paths == ["src/secrets/**"]

    def test_read_path_acl_does_not_authorize_write_paths(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "ml-team",
                    "repos": ["models/*"],
                    "operations": ["clone"],
                    "read_paths": ["src/**"],
                },
            ],
        })

        decision = engine.evaluate(
            identity="bob@corp.example.com",
            groups=["ml-team"],
            repo_url="crab://bucket/models/gpt4",
            operation="push",
            changed_paths=["src/lib.rs"],
        )

        assert not decision.allowed

    def test_write_path_acl_does_not_scope_read_credentials(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "ml-team",
                    "repos": ["models/*"],
                    "operations": ["clone"],
                    "write_paths": ["src/**"],
                },
            ],
        })

        decision = engine.evaluate(
            identity="bob@corp.example.com",
            groups=["ml-team"],
            repo_url="crab://bucket/models/gpt4",
            operation="clone",
        )

        assert decision.allowed
        assert decision.read_paths is None
        assert decision.denied_read_paths == []

    def test_pathless_push_rule_allows_empty_changed_path_set(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "platform-admins",
                    "repos": ["models/*"],
                    "operations": ["push"],
                },
            ],
        })

        decision = engine.evaluate(
            identity="admin@corp.example.com",
            groups=["platform-admins"],
            repo_url="crab://bucket/models/gpt4",
            operation="push",
            changed_paths=[],
        )

        assert decision.allowed

    def test_path_deny_rule_matches_any_changed_path(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "platform-admins",
                    "repos": ["*"],
                    "operations": ["push"],
                    "write_paths": ["*"],
                },
            ],
            "deny": [
                {
                    "identity": "*",
                    "repos": ["*"],
                    "operations": ["push"],
                    "write_paths": ["secrets/**"],
                },
            ],
        })
        decision = engine.evaluate(
            identity="admin@corp.example.com",
            groups=["platform-admins"],
            repo_url="crab://bucket/any/repo",
            operation="push",
            changed_paths=["src/lib.rs", "secrets/prod.env"],
        )
        assert not decision.allowed
        assert "denied" in decision.reason.lower()

    def test_path_acl_does_not_rewrite_changed_paths_before_matching(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "engineers",
                    "repos": ["app/*"],
                    "operations": ["push"],
                    "write_paths": ["src/**"],
                },
            ],
        })
        decision = engine.evaluate(
            identity="dev@corp.example.com",
            groups=["engineers"],
            repo_url="crab://bucket/app/frontend",
            operation="push",
            changed_paths=[" src/lib.rs"],
        )

        assert not decision.allowed
        assert "invalid changed path" in decision.reason

    @pytest.mark.parametrize(
        "changed_path",
        [
            "",
            "src/lib.rs ",
            "/src/lib.rs",
            "src//lib.rs",
            "src/../secret.env",
            "src/a\nb",
        ],
    )
    def test_path_acl_rejects_unsafe_changed_path_shape(self, changed_path):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "engineers",
                    "repos": ["app/*"],
                    "operations": ["push"],
                    "write_paths": ["*"],
                },
            ],
        })
        decision = engine.evaluate(
            identity="dev@corp.example.com",
            groups=["engineers"],
            repo_url="crab://bucket/app/frontend",
            operation="push",
            changed_paths=[changed_path],
        )

        assert not decision.allowed
        assert "invalid changed path" in decision.reason

    def test_ml_team_can_push_to_models(self):
        decision = self.engine.evaluate(
            identity="bob@corp.example.com",
            groups=["ml-team"],
            repo_url="crab://bucket/ml-models/gpt4",
            operation="push",
        )
        assert decision.allowed
        assert decision.provider == "aws"

    def test_resolve_provider_ignores_path_acl_but_requires_single_provider(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "platform-admins",
                    "repos": ["restricted/*"],
                    "operations": ["push"],
                    "write_paths": ["src/**"],
                    "provider": "gcp",
                },
                {
                    "group": "platform-admins",
                    "repos": ["restricted/*"],
                    "operations": ["push"],
                    "write_paths": ["docs/**"],
                    "provider": "gcp",
                },
            ],
        })

        decision = engine.resolve_provider(
            identity="admin@corp.example.com",
            groups=["platform-admins"],
            repo_url="crab://bucket/restricted/repo",
            operation="push",
        )

        assert decision.allowed
        assert decision.provider == "gcp"

    def test_resolve_provider_denies_ambiguous_matching_providers(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "platform-admins",
                    "repos": ["restricted/*"],
                    "operations": ["push"],
                    "write_paths": ["src/**"],
                    "provider": "aws",
                },
                {
                    "group": "platform-admins",
                    "repos": ["restricted/*"],
                    "operations": ["push"],
                    "write_paths": ["docs/**"],
                    "provider": "gcp",
                },
            ],
        })

        decision = engine.resolve_provider(
            identity="admin@corp.example.com",
            groups=["platform-admins"],
            repo_url="crab://bucket/restricted/repo",
            operation="push",
        )

        assert not decision.allowed
        assert "ambiguous provider" in decision.reason

    def test_resolve_provider_honors_full_path_deny_rule(self):
        engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "platform-admins",
                    "repos": ["restricted/*"],
                    "operations": ["push"],
                    "provider": "aws",
                },
            ],
            "deny": [
                {
                    "identity": "admin@corp.example.com",
                    "repos": ["restricted/*"],
                    "operations": ["push"],
                },
            ],
        })

        decision = engine.resolve_provider(
            identity="admin@corp.example.com",
            groups=["platform-admins"],
            repo_url="crab://bucket/restricted/repo",
            operation="push",
        )

        assert not decision.allowed
        assert "denied" in decision.reason

    def test_ml_team_cannot_push_to_unrelated_repo(self):
        decision = self.engine.evaluate(
            identity="bob@corp.example.com",
            groups=["ml-team"],
            repo_url="crab://bucket/infrastructure/terraform",
            operation="push",
        )
        assert not decision.allowed

    def test_individual_identity_match(self):
        decision = self.engine.evaluate(
            identity="alice@corp.example.com",
            groups=[],
            repo_url="crab://bucket/experiments/alice/project1",
            operation="push",
        )
        assert decision.allowed

    def test_individual_identity_no_access_to_others(self):
        decision = self.engine.evaluate(
            identity="alice@corp.example.com",
            groups=[],
            repo_url="crab://bucket/experiments/bob/project1",
            operation="push",
        )
        assert not decision.allowed

    def test_wildcard_identity_public_repos(self):
        decision = self.engine.evaluate(
            identity="anyone@corp.example.com",
            groups=[],
            repo_url="crab://bucket/public/datasets",
            operation="fetch",
        )
        assert decision.allowed
        assert "read" in decision.permissions

    def test_wildcard_identity_cannot_push_to_public(self):
        decision = self.engine.evaluate(
            identity="anyone@corp.example.com",
            groups=[],
            repo_url="crab://bucket/public/datasets",
            operation="push",
        )
        assert not decision.allowed

    def test_deny_rule_takes_precedence(self):
        decision = self.engine.evaluate(
            identity="banned@corp.example.com",
            groups=["platform-admins"],  # Even admins can be denied
            repo_url="crab://bucket/public/data",
            operation="fetch",
        )
        assert not decision.allowed
        assert "denied" in decision.reason.lower()

    def test_no_matching_rule_denies(self):
        decision = self.engine.evaluate(
            identity="stranger@external.com",
            groups=[],
            repo_url="crab://bucket/private/secret",
            operation="fetch",
        )
        assert not decision.allowed
        assert "no matching" in decision.reason.lower()

    def test_invalid_operation_denied(self):
        decision = self.engine.evaluate(
            identity="alice@corp.example.com",
            groups=["platform-admins"],
            repo_url="crab://bucket/any/repo",
            operation="drop_tables",
        )
        assert not decision.allowed

    def test_request_wildcard_operation_denied(self):
        decision = self.engine.evaluate(
            identity="admin@corp.example.com",
            groups=["platform-admins"],
            repo_url="crab://bucket/any/repo",
            operation="*",
        )
        assert not decision.allowed
        assert "invalid operation" in decision.reason.lower()

    def test_case_insensitive_identity_matching(self):
        decision = self.engine.evaluate(
            identity="Alice@Corp.Example.Com",
            groups=[],
            repo_url="crab://bucket/experiments/alice/project1",
            operation="fetch",
        )
        assert decision.allowed

    def test_case_insensitive_group_matching(self):
        decision = self.engine.evaluate(
            identity="bob@corp.example.com",
            groups=["ML-Team"],
            repo_url="crab://bucket/ml-models/gpt4",
            operation="fetch",
        )
        assert decision.allowed


class TestRepoUrlParsing:
    """Test repo URL extraction from various URL formats."""

    def setup_method(self):
        self.engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "identity": "*",
                    "repos": ["team/repo"],
                    "operations": ["fetch"],
                },
            ],
        })

    def test_crab_scheme(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/team/repo",
            operation="fetch",
        )
        assert decision.allowed

    def test_s3_scheme(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="s3://bucket/team/repo",
            operation="fetch",
        )
        assert decision.allowed

    def test_url_with_trailing_whitespace(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="  crab://bucket/team/repo  ",
            operation="fetch",
        )
        assert decision.allowed

    def test_repo_url_without_prefix_is_denied(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket",
            operation="fetch",
        )
        assert not decision.allowed
        assert "invalid repo_url" in decision.reason

    def test_repo_url_with_glob_metacharacter_is_denied(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/team/*",
            operation="fetch",
        )
        assert not decision.allowed
        assert "invalid repo_url" in decision.reason


class TestOperationPermissions:
    """Test that operations map to correct permission sets."""

    def setup_method(self):
        self.engine = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "identity": "*",
                    "repos": ["*"],
                    "operations": ["*"],
                },
            ],
        })

    def test_fetch_gives_read_only(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/repo",
            operation="fetch",
        )
        assert decision.permissions == ["read"]

    def test_clone_gives_read_only(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/repo",
            operation="clone",
        )
        assert decision.permissions == ["read"]

    def test_push_gives_read_write(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/repo",
            operation="push",
        )
        assert "read" in decision.permissions
        assert "write" in decision.permissions

    def test_gc_gives_read_write(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/repo",
            operation="gc",
        )
        assert "read" in decision.permissions
        assert "write" in decision.permissions

    def test_fsck_gives_read_only(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/repo",
            operation="fsck",
        )
        assert decision.permissions == ["read"]

    def test_hydrate_gives_read_only(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/repo",
            operation="hydrate",
        )
        assert decision.permissions == ["read"]

    def test_pull_gives_read_only(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/repo",
            operation="pull",
        )
        assert decision.permissions == ["read"]

    def test_compact_gives_read_write(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/repo",
            operation="compact",
        )
        assert "read" in decision.permissions
        assert "write" in decision.permissions

    def test_optimize_xorbs_gives_read_write(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/repo",
            operation="optimize-xorbs",
        )
        assert decision.permissions == ["read", "write"]

    def test_diff_gives_read_only(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/repo",
            operation="diff",
        )
        assert decision.permissions == ["read"]

    def test_workflow_cache_pull_gives_read_only(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/repo",
            operation="workflow-cache-pull",
        )
        assert decision.permissions == ["read"]

    def test_workflow_push_cache_gives_read_write(self):
        decision = self.engine.evaluate(
            identity="user@example.com",
            groups=[],
            repo_url="crab://bucket/repo",
            operation="workflow-push-cache",
        )
        assert decision.permissions == ["read", "write"]


class TestPolicyLoading:
    """Test policy file loading and validation."""

    def test_missing_file_denies_all(self, tmp_path):
        engine = PolicyEngine.from_file(tmp_path / "nonexistent.yaml")
        decision = engine.evaluate(
            identity="admin@example.com",
            groups=["platform-admins"],
            repo_url="crab://bucket/repo",
            operation="fetch",
        )
        assert not decision.allowed

    def test_load_from_yaml_file(self, tmp_path, sample_policy):
        import yaml

        policy_file = tmp_path / "policy.yaml"
        policy_file.write_text(yaml.dump(sample_policy))

        engine = PolicyEngine.from_file(policy_file)
        decision = engine.evaluate(
            identity="alice@corp.example.com",
            groups=[],
            repo_url="crab://bucket/experiments/alice/test",
            operation="fetch",
        )
        assert decision.allowed

    def test_invalid_version_raises(self):
        with pytest.raises(ValueError, match="Unsupported policy version"):
            PolicyEngine.from_dict({"version": "99", "rules": []})

    def test_unknown_top_level_field_raises(self):
        with pytest.raises(ValueError, match="Unknown policy field"):
            PolicyEngine.from_dict({
                "version": "1",
                "protected_repo": ["restricted/*"],
                "rules": [],
            })

    def test_unknown_rule_field_raises(self):
        with pytest.raises(ValueError, match=r"Unknown policy rule field rules\[0\]\.path"):
            PolicyEngine.from_dict({
                "version": "1",
                "rules": [
                    {
                        "group": "ml-team",
                        "repos": ["models/*"],
                        "operations": ["push"],
                        "path": ["models/**"],
                    },
                ],
            })

    def test_legacy_paths_rule_field_raises(self):
        with pytest.raises(ValueError, match=r"Unknown policy rule field rules\[0\]\.paths"):
            PolicyEngine.from_dict({
                "version": "1",
                "rules": [
                    {
                        "group": "ml-team",
                        "repos": ["models/*"],
                        "operations": ["push"],
                        "paths": ["models/**"],
                    },
                ],
            })

    def test_rule_requires_identity_or_group(self):
        with pytest.raises(ValueError, match="must set identity or group"):
            PolicyEngine.from_dict({
                "version": "1",
                "rules": [
                    {
                        "repos": ["models/*"],
                        "operations": ["push"],
                    },
                ],
            })

    def test_rule_repos_and_operations_must_not_be_empty(self):
        with pytest.raises(ValueError, match=r"rules\[0\]\.repos"):
            PolicyEngine.from_dict({
                "version": "1",
                "rules": [
                    {
                        "group": "ml-team",
                        "repos": [],
                        "operations": ["push"],
                    },
                ],
            })

        with pytest.raises(ValueError, match=r"rules\[0\]\.operations"):
            PolicyEngine.from_dict({
                "version": "1",
                "rules": [
                    {
                        "group": "ml-team",
                        "repos": ["models/*"],
                        "operations": [],
                    },
                ],
            })

    def test_invalid_rule_operation_raises(self):
        with pytest.raises(ValueError, match="invalid operation"):
            PolicyEngine.from_dict({
                "version": "1",
                "rules": [
                    {
                        "group": "ml-team",
                        "repos": ["models/*"],
                        "operations": ["pussh"],
                    },
                ],
            })

    def test_unsupported_provider_raises(self):
        with pytest.raises(ValueError, match="unsupported provider"):
            PolicyEngine.from_dict({
                "version": "1",
                "default_provider": "oracle",
                "rules": [],
            })

        with pytest.raises(ValueError, match=r"rules\[0\]\.provider"):
            PolicyEngine.from_dict({
                "version": "1",
                "rules": [
                    {
                        "group": "ml-team",
                        "repos": ["models/*"],
                        "operations": ["push"],
                        "provider": "oracle",
                    },
                ],
            })

    def test_providers_reports_only_routable_allow_providers(self):
        policy = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "ml-team",
                    "repos": ["models/*"],
                    "operations": ["clone"],
                    "provider": "s3",
                },
                {
                    "group": "platform-admins",
                    "repos": ["internal/*"],
                    "operations": ["clone"],
                },
            ],
        })

        assert policy.providers() == {"aws", "s3"}

    def test_providers_omits_unused_default_provider(self):
        policy = PolicyEngine.from_dict({
            "version": "1",
            "default_provider": "aws",
            "rules": [
                {
                    "group": "ml-team",
                    "repos": ["models/*"],
                    "operations": ["clone"],
                    "provider": "s3",
                },
            ],
        })

        assert policy.providers() == {"s3"}

    @pytest.mark.parametrize(
        "field,pattern",
        [
            ("repos", "/models/*"),
            ("repos", "models//prod"),
            ("repos", "models/../prod"),
            ("read_paths", "/secrets/**"),
            ("read_paths", "secrets//prod.env"),
            ("read_paths", "secrets/../prod.env"),
            ("read_paths", "src/bad\nname.rs"),
            ("write_paths", "/secrets/**"),
            ("write_paths", "secrets//prod.env"),
            ("write_paths", "secrets/../prod.env"),
            ("write_paths", "src/bad\nname.rs"),
        ],
    )
    def test_policy_patterns_reject_unsafe_shapes(self, field, pattern):
        rule = {
            "group": "ml-team",
            "repos": ["models/*"],
            "operations": ["push"],
        }
        rule[field] = [pattern]

        with pytest.raises(ValueError, match=field):
            PolicyEngine.from_dict({
                "version": "1",
                "rules": [rule],
            })
