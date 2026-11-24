#!/usr/bin/env python3
"""Attest the deployed Cloud CDN qualification fixture with read-only Google APIs."""

from __future__ import annotations

import re
import socket
from typing import Any
from urllib.parse import SplitResult, urlsplit

from cdn_deployment_policy_cli import PolicyObservation, PrivateBinding, collect_deployment_policy, deployment_parser
from collect_cdn_fixture_cleanup import api_body, bearer
from run_cdn_transport_checks import FAMILY_ROOTS, origin_bucket

from cdn_qualification import SHA256, decode, deployment_component_digest, matches, public_export_binding, require, scope_parts


PROJECT = re.compile(r"[a-z][a-z0-9-]{4,28}[a-z0-9]")
RESOURCE = re.compile(r"[a-z](?:[a-z0-9-]{0,61}[a-z0-9])?")
COMPUTE = urlsplit("https://compute.googleapis.com")
STORAGE = urlsplit("https://storage.googleapis.com")
RESOURCES = urlsplit("https://cloudresourcemanager.googleapis.com")
IAM = urlsplit("https://iam.googleapis.com")
PUBLIC_MEMBERS = {"allUsers", "allAuthenticatedUsers"}
CACHE_HEADER = "X-Crab-CDN-Cache: {cdn_cache_status}"


def document(base: SplitResult, path: str, authorization: str) -> dict[str, Any]:
    value = decode(api_body(base, path, {"Authorization": authorization, "Accept-Encoding": "identity"}, "Cloud CDN policy read"))
    require(isinstance(value, dict), "Google Cloud returned an invalid policy resource")
    return value


def resource_name(ref: Any, project: str, project_number: str, kind: str) -> str:
    pattern = re.compile(rf"(?:https://(?:compute|www)\.googleapis\.com/compute/v1/)?"
                         rf"projects/(?:{project}|{project_number})/global/{kind}/{RESOURCE.pattern}")
    require(matches(pattern, ref), "Cloud CDN resource reference must remain in the approved project's global route")
    return ref.rsplit("/", 1)[-1]


def verify_frontend(forwarding: dict, hostname: str) -> None:
    require(forwarding["loadBalancingScheme"] == "EXTERNAL_MANAGED" and forwarding["networkTier"] == "PREMIUM"
            and forwarding["IPProtocol"] == "TCP" and forwarding.get("portRange") in {"443", "443-443"}
            and not forwarding.get("allPorts") and not forwarding.get("ports"),
            "Cloud CDN fixture requires a global external HTTPS forwarding rule on port 443")
    addresses = {record[4][0] for record in socket.getaddrinfo(hostname, 443, type=socket.SOCK_STREAM)}
    require(addresses == {forwarding["IPAddress"]}, "Cloud CDN hostname DNS differs from the approved forwarding rule")


def verify_backend(backend: dict, bucket: str, access: str) -> None:
    require(backend["bucketName"] == bucket and backend["enableCdn"] is True and backend.get("compressionMode", "DISABLED") == "DISABLED",
            "Cloud CDN backend must cache the raw representation from the approved bucket")
    headers = [CACHE_HEADER, "Cache-Control: private, no-store"] if access == "private" else [CACHE_HEADER]
    require(sorted(backend["customResponseHeaders"]) == sorted(headers), "Cloud CDN viewer response headers differ from the fixture")
    policy = backend["cdnPolicy"]
    require(policy["cacheMode"] == "USE_ORIGIN_HEADERS" and policy.get("negativeCaching", False) is False
            and policy.get("serveWhileStale") == 0 and not policy.get("negativeCachingPolicy")
            and not policy.get("bypassCacheOnRequestHeaders") and not any(policy.get("cacheKeyPolicy", {}).values()),
            "unsafe Cloud CDN cache policy or variant key")
    keys = policy.get("signedUrlKeyNames", [])
    if access == "private":
        require(isinstance(keys, list) and 1 <= len(keys) <= 3 and len(set(keys)) == len(keys)
                and str(policy.get("signedUrlCacheMaxAgeSec")) == "86400", "private Cloud CDN cache needs trusted viewer keys and the approved signed TTL")
    else:
        require(keys == [], "public Cloud CDN backend must use the approved anonymous cache configuration")


def expected_armor_expressions(hostname: str, prefix: str, protected: bool, access: str) -> tuple[list[str], list[str]]:
    denies = [
        "request.scheme != 'https' || request.method != 'GET' || request.query != ''",
        f"!has(request.headers['host']) || request.headers['host'] != '{hostname}'",
    ]
    if access == "private":
        denies.append("!has(request.headers['cookie']) || !request.headers['cookie'].matches("
                      'R"^Cloud-CDN-Cookie=URLPrefix=[A-Za-z0-9_-]+={0,2}:Expires=[0-9]{1,20}:'
                      'KeyName=[A-Za-z0-9_-]{1,63}:Signature=[A-Za-z0-9_-]{27}=$")')
    denies.extend(("has(request.headers['authorization'])",
                   "has(request.headers['range']) && !request.headers['range'].matches(R\"^bytes=[0-9]+-[0-9]+$\")"))
    repo = "/" + prefix.replace(".", r"\.") + "/"
    routes = [
        (repo + "v2/(capsules|checkpoints|catalogs|pack-layers|history|history/source-roots)/", "[a-f0-9]{2}", 2, (0, 1)),
        (repo + "lfs/objects/", "[a-f0-9]{2}/[a-f0-9]{2}", 5, (0, 1, 3, 4)),
    ]
    if protected:
        routes.append((repo + r"\.crab/(xorbs|shards|capsule-chunk-pages)/", "[a-f0-9]{2}", 2, (0, 1)))
    allows = []
    for route, fanout, width, positions in routes:
        shape = f'request.path.matches(R"^{route}{fanout}/[a-f0-9]{{64}}$")'
        allows.append(shape)
        for offset, position in enumerate(positions):
            alternatives = [f"{'.' * position}{digit}{'.' * (width - position - 1)}/{'.' * offset}{digit}"
                            for digit in "0123456789abcdef"]
            denies.append(f'{shape} && !request.path.matches(R"^{route}({"|".join(alternatives)})")')
    return denies, allows


def verify_armor(policy: dict, hostname: str, prefix: str, protected: bool, access: str) -> None:
    require(policy["type"] == "CLOUD_ARMOR_EDGE", "Cloud CDN requires a pre-cache Cloud Armor edge policy")
    denies, allows = expected_armor_expressions(hostname, prefix, protected, access)
    expected = {100 + index: ("deny(403)", expression) for index, expression in enumerate(denies)}
    expected.update({1000 + index: ("allow", expression) for index, expression in enumerate(allows)})
    expected[2147483647] = ("deny(403)", {"versionedExpr": "SRC_IPS_V1", "config": {"srcIpRanges": ["*"]}})
    rules = policy["rules"]
    observed = {}
    for rule in rules:
        match = rule["match"]
        require(not rule.get("preview"), "Cloud Armor fixture rules must be enforced")
        if "expr" in match:
            require(not match.get("config") and match.get("versionedExpr", "SRC_IPS_V1") == "SRC_IPS_V1",
                    "unexpected Cloud Armor matcher configuration")
            match = match["expr"]["expression"]
        observed[rule["priority"]] = (rule["action"], match)
    require(len(rules) == len(expected) and observed == expected, "Cloud Armor rules differ from the exact deployed fixture route policy")


def bucket_condition(bucket: str, prefix: str, protected: bool) -> str:
    families = set().union(*(FAMILY_ROOTS[name] for name in ("git", "capsule", "lfs"))) - {"v2/history/source-roots"}
    if protected:
        families |= set().union(*(FAMILY_ROOTS[name] for name in ("xorb", "chunk-page", "shard")))
    clauses = [f"resource.name.startsWith('projects/_/buckets/{bucket}/objects/{prefix}/{family}/')" for family in sorted(families)]
    return "resource.type == 'storage.googleapis.com/Object' && (" + " || ".join(clauses) + ")"


def condition_matches(binding: dict, expected: str) -> bool:
    # Terraform emits the same prefix disjunction in route-family order.
    head, _, tail = expected.partition(" && (")
    expression = binding.get("condition", {}).get("expression", "")
    return (expression.startswith(head + " && (") and expression.endswith(")")
            and sorted(expression[len(head) + 5:-1].split(" || ")) == sorted(tail[:-1].split(" || ")))


def roles_for_public_export(bindings: list[dict], project: str, expected: str, authorization: str) -> dict[str, Any]:
    anonymous = [row for row in bindings if PUBLIC_MEMBERS.intersection(row["members"])]
    require(len(anonymous) == 1 and anonymous[0]["members"] == ["allUsers"] and not anonymous[0].get("condition"),
            "public GCS export needs one anonymous object GET binding")
    writer = [row for row in bindings if condition_matches(row, expected)]
    require(len(writer) == 1 and len(writer[0]["members"]) == 1 and writer[0]["members"][0].startswith("serviceAccount:")
            and sum(writer[0]["members"][0] in row["members"] for row in bindings) == 1,
            "GCS export writer must have only the approved bucket binding")
    observed = {}
    for name, binding, permissions in (("anonymous", anonymous[0], {"storage.objects.get"}),
                                       ("writer", writer[0], {"storage.objects.get", "storage.objects.create"})):
        role = binding["role"]
        require(matches(re.compile(rf"projects/{project}/roles/[A-Za-z0-9_.]{{3,64}}"), role), "GCS export needs project-owned custom IAM roles")
        value = document(IAM, f"/v1/{role}", authorization)
        require(value["name"] == role and set(value["includedPermissions"]) == permissions
                and not value.get("deleted") and value.get("stage") != "DISABLED",
                "GCS export IAM permissions differ from anonymous GET and create-only publication")
        observed[name] = value
    return observed


def bucket_access(bucket: str, project: str, project_number: str, prefix: str, protected: bool, access: str,
                  authorization: str) -> dict[str, Any]:
    path = f"/storage/v1/b/{bucket}"
    metadata = document(STORAGE, path, authorization)
    config = metadata["iamConfiguration"]
    require(metadata["name"] == bucket and str(metadata["projectNumber"]) == project_number
            and config["uniformBucketLevelAccess"]["enabled"] is True,
            "Cloud CDN origin must be the approved project's bucket with uniform IAM")
    policy = document(STORAGE, path + "/iam?optionsRequestedPolicyVersion=3", authorization)
    require(policy["version"] == 3 and isinstance(policy["bindings"], list), "Cloud Storage must return conditional IAM policy version 3")
    expected = bucket_condition(bucket, prefix, protected)
    bindings = policy["bindings"]
    if access == "private":
        require(config["publicAccessPrevention"] == "enforced" and all(not PUBLIC_MEMBERS.intersection(row["members"]) for row in bindings),
                "private Cloud CDN bucket must enforce public access prevention")
        fill = f"serviceAccount:service-{project_number}@cloud-cdn-fill.iam.gserviceaccount.com"
        grants = [row for row in bindings if fill in row["members"]]
        require(len(grants) == 1 and grants[0]["role"] == "roles/storage.objectViewer" and condition_matches(grants[0], expected),
                "Cloud CDN fill identity must be confined to the approved immutable prefixes at the bucket")
        roles = {}
    else:
        labels = metadata.get("labels", {})
        inventory = labels.get("crab_inventory_0", "") + labels.get("crab_inventory_1", "")
        require(config["publicAccessPrevention"] == "inherited" and labels.get("crab_public_export") == "true"
                and matches(SHA256, inventory) and metadata.get("versioning", {}).get("enabled", False) is False
                and str(metadata.get("softDeletePolicy", {}).get("retentionDurationSeconds")) == "0",
                "GCS public export needs its approved inventory and disabled versioning and soft delete")
        roles = roles_for_public_export(bindings, project, expected, authorization)
    return {"bucket": metadata, "iam": policy, "roles": roles}


def collect(fixture: dict, project: str, forwarding_rule: str, configuration: str | None) -> PolicyObservation:
    edge, access, _, _, _ = scope_parts(fixture["scope"])
    require(edge == "cloud-cdn" and matches(PROJECT, project) and matches(RESOURCE, forwarding_rule),
            "Cloud CDN attestation requires an approved GCP project and global forwarding rule")
    base = urlsplit(fixture["base_url"])
    require(base.netloc == base.hostname and base.path in {"", "/"}, "Cloud CDN reference fixture uses a dedicated HTTPS hostname")
    credentials = [f"--configuration={configuration}"] if configuration is not None else []
    authorization = bearer("gcloud", *credentials, "auth", "print-access-token", "--quiet")
    identity = document(RESOURCES, f"/v1/projects/{project}", authorization)
    project_number = str(identity["projectNumber"])
    require(identity["projectId"] == project and re.fullmatch(r"[0-9]+", project_number), "wrong GCP project identity")

    def compute(kind: str, ref: str) -> dict[str, Any]:
        name = resource_name(ref, project, project_number, kind)
        value = document(COMPUTE, f"/compute/v1/projects/{project}/global/{kind}/{name}", authorization)
        require(value["name"] == name and resource_name(value["selfLink"], project, project_number, kind) == name,
                "Cloud CDN resource identity differs from its approved global route")
        return value

    forwarding_ref = f"projects/{project}/global/forwardingRules/{forwarding_rule}"
    forwarding = compute("forwardingRules", forwarding_ref)
    verify_frontend(forwarding, base.hostname)
    proxy = compute("targetHttpsProxies", forwarding["target"])
    certificates = proxy["sslCertificates"]
    require(len(certificates) == 1 and not any(proxy.get(name) for name in ("certificateMap", "sslPolicy", "serverTlsPolicy", "authorizationPolicy")),
            "Cloud CDN reference proxy needs one Google-managed HTTPS certificate")
    cert = compute("sslCertificates", certificates[0])
    require(cert["type"] == "MANAGED" and cert["managed"]["status"] == "ACTIVE" and cert["managed"]["domains"] == [base.hostname],
            "Cloud CDN certificate must be active for the exact approved hostname")
    url_map = compute("urlMaps", proxy["urlMap"])
    require(not any(url_map.get(name) for name in ("hostRules", "pathMatchers", "defaultUrlRedirect", "defaultRouteAction",
                                                  "headerAction", "defaultCustomErrorResponsePolicy")),
            "Cloud CDN URL map must send every host and path through the same edge policy")
    backend = compute("backendBuckets", url_map["defaultService"])
    bucket = origin_bucket(fixture["export_origin"] if access == "public" else fixture["native_origin"], edge)
    verify_backend(backend, bucket, access)
    policy = compute("securityPolicies", backend["edgeSecurityPolicy"])
    protected = bool(fixture["view_suffix"])
    prefix = fixture["fixture_prefix"] + fixture["view_suffix"]
    verify_armor(policy, base.hostname, prefix, protected, access)
    storage = bucket_access(bucket, project, project_number, prefix, protected, access, authorization)
    require(compute("forwardingRules", forwarding_ref) == forwarding, "Cloud CDN forwarding rule changed during attestation")
    # Managed certificates and bucket metageneration can change during ordinary renewals and maintenance.
    certificate_binding = {name: cert[name] for name in ("id", "selfLink", "type", "managed")}
    bucket_settings = {name: storage["bucket"].get(name) for name in (
        "id", "name", "projectNumber", "iamConfiguration", "labels", "versioning", "softDeletePolicy", "encryption",
    )}
    deployment = f"projects/{project}/global/backendBuckets/{backend['name']}"
    export_binding = {}
    private_binding = None
    if access == "public":
        labels = storage["bucket"]["labels"]
        inventory = labels["crab_inventory_0"] + labels["crab_inventory_1"]
        source_bucket = origin_bucket(fixture["native_origin"], edge)
        source = document(STORAGE, f"/storage/v1/b/{source_bucket}", authorization)
        require(source["name"] == source_bucket, "GCS live source does not match its native bucket name")
        export_binding["public-export"] = public_export_binding(
            fixture, deployment, inventory,
            {"resource": f"projects/{source['projectNumber']}/buckets/{source_bucket}", "location": source["location"]},
            {"resource": f"projects/{project_number}/buckets/{bucket}", "location": storage["bucket"]["location"]},
        )
    else:
        location = storage["bucket"].get("location")
        require(matches(re.compile(r"[A-Z0-9-]{1,40}"), location), "GCS native bucket needs an observed location")
        private_binding = PrivateBinding(deployment, f"projects/{project_number}/buckets/{bucket}", location)
    components = {name: deployment_component_digest(value) for name, value in {
        "https-route": {"forwarding": forwarding, "proxy": proxy, "certificate": certificate_binding, "url_map": url_map},
        "backend-cache": backend, "viewer-policy": policy, "bucket-access": storage | {"bucket": bucket_settings},
        "viewer-key-ids": sorted(backend["cdnPolicy"].get("signedUrlKeyNames", [])), **export_binding,
    }.items()}
    return PolicyObservation(components, private_binding)


def main() -> int:
    parser = deployment_parser(__doc__)
    parser.add_argument("--project", required=True)
    parser.add_argument("--forwarding-rule", required=True)
    parser.add_argument("--configuration", help="gcloud configuration with read-only fixture access")
    args = parser.parse_args()
    return collect_deployment_policy(args, "Cloud CDN", lambda fixture: collect(fixture, args.project, args.forwarding_rule, args.configuration))


if __name__ == "__main__":
    raise SystemExit(main())
