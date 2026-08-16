"""RBAC policy engine for the crab-auth endpoint.

Evaluates a YAML policy file to determine whether a given identity
(user or group member) is allowed to perform an operation on a repository.

Deny rules are evaluated before allow grants. Matching allow rules for the
same identity, repo, and operation are unioned so enterprises can compose
group and identity grants without depending on rule order.
"""

from __future__ import annotations

import fnmatch
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import yaml
import structlog

from src.repo_url import RepoUrlError, parse_repo_url

logger = structlog.get_logger()

POLICY_KEYS = frozenset({"version", "default_provider", "protected_repos", "rules", "deny"})
RULE_KEYS = frozenset({
    "identity",
    "group",
    "repos",
    "operations",
    "read_paths",
    "write_paths",
    "provider",
})
SUPPORTED_PROVIDERS = frozenset({"aws", "s3", "gcp", "azure"})

# Valid operations that crab CLI can request.
# Core operations that modify remote state:
#   push, gc, repack, compact, lock
# Core operations that read remote state:
#   fetch, clone, hydrate, mount, fsck
# Auxiliary operations (diagnostics, LFS, filter):
#   du, doctor, metadb, lfs, smudge, ship:manifest-check, pull,
#   clone:shard-sync, optimize-xorbs, tier, diff, workflow-cache-pull,
#   workflow-push-cache
VALID_OPERATIONS = frozenset({
    # Write operations (need read + write permissions)
    "push",
    "gc",
    "repack",
    "compact",
    "lock",
    "lfs",
    "metadb",
    "optimize-xorbs",
    "tier",
    "workflow-push-cache",
    # Read operations (need read permissions only)
    "fetch",
    "clone",
    "clone:shard-sync",
    "diff",
    "hydrate",
    "pull",
    "mount",
    "fsck",
    "du",
    "doctor",
    "smudge",
    "ship:manifest-check",
    "prune",
    "workflow-cache-pull",
})


@dataclass
class PolicyDecision:
    """Result of a policy evaluation."""

    allowed: bool
    reason: str
    provider: str = "aws"
    permissions: list[str] = field(default_factory=list)
    protected_repo: bool = False
    read_paths: list[str] | None = None
    denied_read_paths: list[str] = field(default_factory=list)


@dataclass
class PolicyRule:
    """A single RBAC rule from the policy file."""

    identity: str | None = None
    group: str | None = None
    repos: list[str] = field(default_factory=list)
    operations: list[str] = field(default_factory=list)
    read_paths: list[str] | None = None
    write_paths: list[str] | None = None
    provider: str | None = None


class PolicyEngine:
    """Evaluates RBAC policy rules against incoming requests."""

    def __init__(
        self,
        rules: list[PolicyRule],
        deny_rules: list[PolicyRule],
        protected_repos: list[str] | None = None,
        default_provider: str = "aws",
    ) -> None:
        self._rules = rules
        self._deny_rules = deny_rules
        self._protected_repos = protected_repos or []
        self._default_provider = default_provider

    @classmethod
    def from_file(cls, path: str | Path) -> PolicyEngine:
        """Load policy from a YAML file."""
        path = Path(path)
        if not path.exists():
            logger.warning("policy_file_not_found", path=str(path))
            # No policy file = deny all.
            return cls(
                rules=[],
                deny_rules=[],
                protected_repos=[],
                default_provider="aws",
            )

        with open(path) as f:
            data = yaml.safe_load(f)

        if not isinstance(data, dict):
            raise ValueError(f"Policy file must be a YAML mapping, got {type(data)}")

        version = data.get("version", "1")
        if version != "1":
            raise ValueError(f"Unsupported policy version: {version}")

        _validate_policy_keys(data)
        default_provider = _parse_provider(data.get("default_provider", "aws"), "default_provider")
        rules = _parse_rules(data.get("rules", []), "rules")
        deny_rules = _parse_rules(data.get("deny", []), "deny")
        protected_repos = _parse_pattern_list(
            data.get("protected_repos", []),
            "protected_repos",
            pattern_kind="repo",
            allow_empty=True,
        )

        logger.info(
            "policy_loaded",
            path=str(path),
            rules=len(rules),
            deny_rules=len(deny_rules),
            protected_repos=len(protected_repos),
            default_provider=default_provider,
        )

        return cls(
            rules=rules,
            deny_rules=deny_rules,
            protected_repos=protected_repos,
            default_provider=default_provider,
        )

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> PolicyEngine:
        """Load policy from a dictionary (useful for testing)."""
        version = data.get("version", "1")
        if version != "1":
            raise ValueError(f"Unsupported policy version: {version}")

        _validate_policy_keys(data)
        default_provider = _parse_provider(data.get("default_provider", "aws"), "default_provider")
        rules = _parse_rules(data.get("rules", []), "rules")
        deny_rules = _parse_rules(data.get("deny", []), "deny")
        protected_repos = _parse_pattern_list(
            data.get("protected_repos", []),
            "protected_repos",
            pattern_kind="repo",
            allow_empty=True,
        )
        return cls(
            rules=rules,
            deny_rules=deny_rules,
            protected_repos=protected_repos,
            default_provider=default_provider,
        )

    def providers(self) -> set[str]:
        """Return credential providers that allow rules may route to."""
        providers = {rule.provider or self._default_provider for rule in self._rules}
        if not providers:
            providers.add(self._default_provider)
        return providers

    def evaluate(
        self,
        identity: str,
        groups: list[str],
        repo_url: str,
        operation: str,
        changed_paths: list[str] | None = None,
    ) -> PolicyDecision:
        """Evaluate the policy for a given request.

        Returns a PolicyDecision indicating whether access is allowed,
        which cloud provider to use, and what permissions to grant.
        """
        # Normalize the repo path from the URL.
        try:
            repo_path = _extract_repo_path(repo_url)
        except ValueError as e:
            return PolicyDecision(
                allowed=False,
                reason=f"invalid repo_url: {e}",
            )
        protected_repo = _matches_any_repo(self._protected_repos, repo_path)
        try:
            normalized_changed_paths = _normalize_changed_paths(changed_paths)
        except ValueError as e:
            return PolicyDecision(
                allowed=False,
                reason=f"invalid changed path: {e}",
            )

        # Validate operation.
        if operation not in VALID_OPERATIONS:
            return PolicyDecision(
                allowed=False,
                reason=f"invalid operation: {operation}",
            )

        if _is_write_operation(operation):
            return self._evaluate_write(
                identity=identity,
                groups=groups,
                repo_path=repo_path,
                repo_url=repo_url,
                operation=operation,
                protected_repo=protected_repo,
                changed_paths=normalized_changed_paths,
            )
        return self._evaluate_read(
            identity=identity,
            groups=groups,
            repo_path=repo_path,
            repo_url=repo_url,
            operation=operation,
            protected_repo=protected_repo,
        )

    def resolve_provider(
        self,
        identity: str,
        groups: list[str],
        repo_url: str,
        operation: str,
    ) -> PolicyDecision:
        """Resolve the storage provider for a request without trusting paths.

        Path ACLs are evaluated separately with verified changed paths. Provider
        routing is repo-level; if matching allow rules disagree on provider, the
        service fails closed instead of guessing which cloud owns the repo.
        """
        try:
            repo_path = _extract_repo_path(repo_url)
        except ValueError as e:
            return PolicyDecision(
                allowed=False,
                reason=f"invalid repo_url: {e}",
            )
        protected_repo = _matches_any_repo(self._protected_repos, repo_path)

        if operation not in VALID_OPERATIONS:
            return PolicyDecision(
                allowed=False,
                reason=f"invalid operation: {operation}",
            )

        path_attr = _path_attr_for_operation(operation)
        for rule in self._deny_rules:
            if getattr(rule, path_attr) is None and _rule_matches_without_paths(
                rule, identity, groups, repo_path, operation
            ):
                return PolicyDecision(
                    allowed=False,
                    reason="explicitly denied by deny rule",
                )

        providers = {
            rule.provider or self._default_provider
            for rule in self._rules
            if _rule_matches_without_paths(rule, identity, groups, repo_path, operation)
        }
        if not providers:
            return PolicyDecision(
                allowed=False,
                reason="no matching policy rule",
            )
        if len(providers) != 1:
            return PolicyDecision(
                allowed=False,
                reason="ambiguous provider for matching policy rules",
            )

        permissions = _operation_to_permissions(operation)
        return PolicyDecision(
            allowed=True,
            reason="matched provider rule",
            provider=next(iter(providers)),
            permissions=_protect_write_permissions(permissions, protected_repo),
            protected_repo=protected_repo,
        )

    def _evaluate_read(
        self,
        *,
        identity: str,
        groups: list[str],
        repo_path: str,
        repo_url: str,
        operation: str,
        protected_repo: bool,
    ) -> PolicyDecision:
        denied_read_paths: list[str] = []
        for rule in self._deny_rules:
            if not _rule_matches_without_paths(rule, identity, groups, repo_path, operation):
                continue
            if rule.read_paths is None:
                return PolicyDecision(
                    allowed=False,
                    reason="explicitly denied by deny rule",
                )
            denied_read_paths.extend(rule.read_paths)

        matching_rules = [
            rule
            for rule in self._rules
            if _rule_matches_without_paths(rule, identity, groups, repo_path, operation)
        ]
        if not matching_rules:
            return PolicyDecision(
                allowed=False,
                reason="no matching policy rule",
            )

        provider_decision = _provider_for_rules(
            matching_rules,
            self._default_provider,
        )
        if not provider_decision.allowed:
            return provider_decision

        repo_wide = any(rule.read_paths is None for rule in matching_rules)
        allowed_paths = _unique_patterns(
            pattern
            for rule in matching_rules
            if rule.read_paths is not None
            for pattern in rule.read_paths
        )
        if not repo_wide and not allowed_paths:
            return PolicyDecision(
                allowed=False,
                reason="no matching read path policy rule",
            )

        permissions = _operation_to_permissions(operation)
        return PolicyDecision(
            allowed=True,
            reason="matched allow rule",
            provider=provider_decision.provider,
            permissions=_protect_write_permissions(permissions, protected_repo),
            protected_repo=protected_repo,
            read_paths=None if repo_wide else allowed_paths,
            denied_read_paths=_unique_patterns(denied_read_paths),
        )

    def _evaluate_write(
        self,
        *,
        identity: str,
        groups: list[str],
        repo_path: str,
        repo_url: str,
        operation: str,
        protected_repo: bool,
        changed_paths: list[str] | None,
    ) -> PolicyDecision:
        del repo_url
        for rule in self._deny_rules:
            if not _rule_matches_without_paths(rule, identity, groups, repo_path, operation):
                continue
            if rule.write_paths is None:
                return PolicyDecision(
                    allowed=False,
                    reason="explicitly denied by deny rule",
                )
            if _paths_match(rule.write_paths, changed_paths, path_mode="any"):
                return PolicyDecision(
                    allowed=False,
                    reason="explicitly denied by deny rule",
                )

        matching_rules = [
            rule
            for rule in self._rules
            if _rule_matches_without_paths(rule, identity, groups, repo_path, operation)
        ]
        if not matching_rules:
            return PolicyDecision(
                allowed=False,
                reason="no matching policy rule",
            )

        provider_decision = _provider_for_rules(
            matching_rules,
            self._default_provider,
        )
        if not provider_decision.allowed:
            return provider_decision

        repo_wide = any(rule.write_paths is None for rule in matching_rules)
        allowed_paths = _unique_patterns(
            pattern
            for rule in matching_rules
            if rule.write_paths is not None
            for pattern in rule.write_paths
        )
        if not repo_wide and not _paths_match(
            allowed_paths,
            changed_paths,
            path_mode="all",
        ):
            return PolicyDecision(
                allowed=False,
                reason="no matching write path policy rule",
            )

        permissions = _operation_to_permissions(operation)
        return PolicyDecision(
            allowed=True,
            reason="matched allow rule",
            provider=provider_decision.provider,
            permissions=_protect_write_permissions(permissions, protected_repo),
            protected_repo=protected_repo,
        )


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------


def _validate_policy_keys(data: dict[str, Any]) -> None:
    unknown = sorted(set(data) - POLICY_KEYS)
    if unknown:
        raise ValueError(f"Unknown policy field: {unknown[0]}")


def _parse_rules(value: Any, field: str) -> list[PolicyRule]:
    if value is None:
        return []
    if not isinstance(value, list):
        raise ValueError(f"Policy field {field} must be a list")
    return [_parse_rule(rule, f"{field}[{idx}]") for idx, rule in enumerate(value)]


def _parse_rule(data: Any, location: str) -> PolicyRule:
    """Parse a rule dictionary from the policy YAML."""
    if not isinstance(data, dict):
        raise ValueError(f"Policy rule {location} must be a mapping")
    unknown = sorted(set(data) - RULE_KEYS)
    if unknown:
        raise ValueError(f"Unknown policy rule field {location}.{unknown[0]}")

    identity = data.get("identity")
    group = data.get("group")
    if identity is None and group is None:
        raise ValueError(f"Policy rule {location} must set identity or group")
    if identity is not None and not isinstance(identity, str):
        raise ValueError(f"Policy rule {location}.identity must be a string")
    if group is not None and not isinstance(group, str):
        raise ValueError(f"Policy rule {location}.group must be a string")

    repos = _parse_pattern_list(data.get("repos", []), f"{location}.repos", pattern_kind="repo")
    operations = _parse_operations(data.get("operations", []), f"{location}.operations")
    read_paths = None
    if "read_paths" in data:
        read_paths = _parse_pattern_list(
            data.get("read_paths"), f"{location}.read_paths", pattern_kind="path"
        )
    write_paths = None
    if "write_paths" in data:
        write_paths = _parse_pattern_list(
            data.get("write_paths"), f"{location}.write_paths", pattern_kind="path"
        )

    return PolicyRule(
        identity=identity,
        group=group,
        repos=repos,
        operations=operations,
        read_paths=read_paths,
        write_paths=write_paths,
        provider=_parse_optional_provider(data.get("provider"), f"{location}.provider"),
    )


def _parse_pattern_list(
    value: Any,
    field: str,
    *,
    pattern_kind: str,
    allow_empty: bool = False,
) -> list[str]:
    if isinstance(value, str):
        values = [value]
    elif isinstance(value, list):
        values = value
    else:
        raise ValueError(f"Policy field {field} must be a string or list of strings")

    if not values and not allow_empty:
        raise ValueError(f"Policy field {field} must not be empty")

    parsed = []
    for idx, item in enumerate(values):
        if not isinstance(item, str):
            raise ValueError(f"Policy field {field}[{idx}] must be a string")
        pattern = item.strip()
        if pattern != item or not pattern:
            raise ValueError(f"Policy field {field}[{idx}] has invalid whitespace")
        if pattern_kind == "path":
            _validate_path_pattern(pattern, f"{field}[{idx}]")
        else:
            _validate_repo_pattern(pattern, f"{field}[{idx}]")
        parsed.append(pattern)
    return parsed


def _parse_operations(value: Any, field: str) -> list[str]:
    if isinstance(value, str):
        values = [value]
    elif isinstance(value, list):
        values = value
    else:
        raise ValueError(f"Policy field {field} must be a string or list of strings")
    if not values:
        raise ValueError(f"Policy field {field} must not be empty")

    operations = []
    for idx, item in enumerate(values):
        if not isinstance(item, str) or not item:
            raise ValueError(f"Policy field {field}[{idx}] must be a non-empty string")
        op = item.lower()
        if op != "*" and op not in VALID_OPERATIONS:
            raise ValueError(f"Policy field {field}[{idx}] has invalid operation: {item}")
        operations.append(op)
    return operations


def _parse_provider(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value:
        raise ValueError(f"Policy field {field} must be a non-empty string")
    provider = value.lower()
    if provider not in SUPPORTED_PROVIDERS:
        raise ValueError(f"Policy field {field} has unsupported provider: {value}")
    return provider


def _parse_optional_provider(value: Any, field: str) -> str | None:
    if value is None:
        return None
    return _parse_provider(value, field)


def _validate_repo_pattern(pattern: str, field: str) -> None:
    if pattern == "*":
        return
    if _has_unsafe_pattern_shape(pattern):
        raise ValueError(f"Policy field {field} has unsafe repo pattern")


def _validate_path_pattern(pattern: str, field: str) -> None:
    if pattern == "*":
        return
    if _has_unsafe_pattern_shape(pattern):
        raise ValueError(f"Policy field {field} has unsafe path pattern")


def _has_unsafe_pattern_shape(pattern: str) -> bool:
    return (
        pattern.startswith("/")
        or pattern.endswith("/")
        or "//" in pattern
        or any(ord(ch) < 32 or ord(ch) == 127 for ch in pattern)
        or any(segment in {"", ".", ".."} for segment in pattern.split("/"))
    )


def _rule_matches_without_paths(
    rule: PolicyRule,
    identity: str,
    groups: list[str],
    repo_path: str,
    operation: str,
) -> bool:
    return (
        _identity_matches(rule, identity, groups)
        and _repo_matches(rule, repo_path)
        and _operation_matches(rule, operation)
    )


def _identity_matches(rule: PolicyRule, identity: str, groups: list[str]) -> bool:
    if rule.identity is not None:
        if rule.identity == "*":
            return True
        if fnmatch.fnmatch(identity.lower(), rule.identity.lower()):
            return True

    if rule.group is not None:
        if rule.group == "*":
            return True
        if rule.group.lower() in [g.lower() for g in groups]:
            return True

    return False


def _repo_matches(rule: PolicyRule, repo_path: str) -> bool:
    for pattern in rule.repos:
        if pattern == "*":
            return True
        if fnmatch.fnmatch(repo_path.lower(), pattern.lower()):
            return True
    return False


def _operation_matches(rule: PolicyRule, operation: str) -> bool:
    return "*" in rule.operations or operation.lower() in [
        op.lower() for op in rule.operations
    ]


def _paths_match(
    patterns: list[str] | None,
    changed_paths: list[str] | None,
    path_mode: str,
) -> bool:
    if patterns is None:
        return True
    if changed_paths is None:
        return False
    if not changed_paths:
        return False

    if path_mode == "any":
        return any(_matches_any_path(patterns, path) for path in changed_paths)
    return all(_matches_any_path(patterns, path) for path in changed_paths)


def _provider_for_rules(
    rules: list[PolicyRule],
    default_provider: str,
) -> PolicyDecision:
    providers = {rule.provider or default_provider for rule in rules}
    if len(providers) != 1:
        return PolicyDecision(
            allowed=False,
            reason="ambiguous provider for matching policy rules",
        )
    return PolicyDecision(allowed=True, reason="matched provider rule", provider=next(iter(providers)))


def _unique_patterns(patterns) -> list[str]:
    seen: set[str] = set()
    result: list[str] = []
    for pattern in patterns:
        if pattern in seen:
            continue
        seen.add(pattern)
        result.append(pattern)
    return result


def _matches_any_path(patterns: list[str], changed_path: str) -> bool:
    for pattern in patterns:
        pattern = pattern.strip("/")
        if pattern == "*":
            return True
        if fnmatch.fnmatch(changed_path.lower(), pattern.lower()):
            return True
    return False


def _normalize_changed_paths(changed_paths: list[str] | None) -> list[str] | None:
    if changed_paths is None:
        return None

    normalized = []
    for path in changed_paths:
        if not isinstance(path, str):
            raise ValueError("path must be a string")
        if not path:
            raise ValueError("empty path")
        if (
            path != path.strip()
            or path.startswith("/")
            or path.endswith("/")
            or "//" in path
            or any(ord(ch) < 32 or ord(ch) == 127 for ch in path)
        ):
            raise ValueError(path)
        segments = path.split("/")
        if (
            any(segment in {"", ".", ".."} for segment in segments)
            or len(path) > 4096
        ):
            raise ValueError(path)
        normalized.append(path)
    return normalized


def _matches_any_repo(patterns: list[str], repo_path: str) -> bool:
    for pattern in patterns:
        if pattern == "*":
            return True
        if fnmatch.fnmatch(repo_path.lower(), pattern.lower()):
            return True
    return False


def _protect_write_permissions(
    permissions: list[str], protected_repo: bool
) -> list[str]:
    if not protected_repo or "write" not in permissions:
        return permissions
    return ["immutable-write" if p == "write" else p for p in permissions]


def _extract_repo_path(repo_url: str) -> str:
    """Extract the repo path from a crab:// URL.

    Examples:
        crab://bucket/repo/path → repo/path
        crab://ml-models/team-alpha/gpt4 → team-alpha/gpt4
    """
    try:
        return parse_repo_url(repo_url).prefix
    except RepoUrlError as e:
        raise ValueError(str(e)) from e


def _operation_to_permissions(operation: str) -> list[str]:
    """Map an operation to the permissions array in the response.

    Write operations need both read and write access to the bucket.
    Read operations only need read access.
    """
    if _is_write_operation(operation):
        return ["read", "write"]
    return ["read"]


def _is_write_operation(operation: str) -> bool:
    write_operations = frozenset({
        "push", "gc", "repack", "compact", "lock", "lfs", "metadb",
        "optimize-xorbs", "tier", "workflow-push-cache",
    })
    return operation in write_operations


def _path_attr_for_operation(operation: str) -> str:
    if _is_write_operation(operation):
        return "write_paths"
    return "read_paths"
