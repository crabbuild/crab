#!/usr/bin/env python3
"""Attest private APIM and public Front Door qualification fixtures through Azure ARM."""

from __future__ import annotations

import base64
import ipaddress
import re
from collections.abc import Callable
from pathlib import Path
from typing import Any
from urllib.parse import urlencode, urlsplit
from xml.etree import ElementTree

from cdn_deployment_policy_cli import (
    PolicyDeploymentPending, PolicyObservation, PrivateBinding, collect_deployment_policy, deployment_parser,
)
from collect_cdn_fixture_cleanup import api_body, bearer
from run_cdn_transport_checks import origin_bucket

from cdn_qualification import (
    SHA256, CdnEvidenceError, decode, deployment_component_digest, matches, public_export_binding, require, scope_parts,
)


ARM = urlsplit("https://management.azure.com")
AZURE_TEMPLATES = Path(__file__).resolve().parents[2] / "deploy/cdn/azure"
SUBSCRIPTION = re.compile(r"[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}", re.IGNORECASE)
RESOURCE_GROUP = re.compile(r"[A-Za-z0-9._()-]{1,90}")
PROFILE = re.compile(r"[A-Za-z0-9]+(?:-+[A-Za-z0-9]+)*")
FRONT_DOOR_HOST = re.compile(r"[a-z0-9-]+(?:\.[a-z0-9-]+)*\.azurefd\.net")
FRONT_DOOR = "2025-06-01"
APIM = "2024-05-01"
BLOB = "2025-01-01"
WAF = "2025-03-01"
RBAC = "2022-04-01"
BLOB_READER = "2a2b9908-6ea1-4ae2-8e65-a410df84e7d1"


def arm_document(path: str, version: str, authorization: str, **options: str) -> dict[str, Any]:
    query = urlencode({"api-version": version} | options)
    value = decode(api_body(ARM, f"{path}?{query}", {"Authorization": authorization, "Accept-Encoding": "identity"},
                            "Azure Front Door policy read"))
    require(isinstance(value, dict), "Azure returned an invalid policy resource")
    return value


def arm_entries(response: dict[str, Any]) -> list[dict[str, Any]]:
    entries = response["value"]
    require(isinstance(entries, list) and not response.get("nextLink") and response.get("count", len(entries)) == len(entries),
            "Azure resource inventory is incomplete")
    return entries


def resource(value: dict[str, Any], expected_id: str) -> dict[str, Any]:
    require(value["id"].lower() == expected_id.lower(), "Azure resource identity differs from the approved fixture")
    return value


def deployed(value: dict[str, Any]) -> None:
    props = value["properties"]
    provisioning = props["provisioningState"]
    deployment = props.get("deploymentStatus", "Succeeded")
    if provisioning == deployment == "Succeeded":
        return
    if provisioning in ("Succeeded", "Updating") and deployment in ("NotStarted", "InProgress", "Succeeded"):
        raise PolicyDeploymentPending("Azure fixture deployment is in progress")
    raise CdnEvidenceError("Azure fixture resource must be provisioned and deployed")


def one_resource(entries: list[dict[str, Any]], expected_id: str, error: str) -> dict[str, Any]:
    require(len(entries) == 1, error)
    return resource(entries[0], expected_id)


def front_door_route(read: Callable, profile_id: str, profile_name: str, host: str, container: str, access: str) -> dict:
    profile = resource(read(profile_id, FRONT_DOOR), profile_id)
    deployed(profile)
    require(profile["sku"]["name"] == "Premium_AzureFrontDoor", "qualification profile needs Front Door Premium")
    endpoint_id = f"{profile_id}/afdEndpoints/{profile_name}"
    endpoint = one_resource(arm_entries(read(f"{profile_id}/afdEndpoints", FRONT_DOOR)), endpoint_id,
                            "qualification profile needs one Front Door endpoint")
    deployed(endpoint)
    hostname = endpoint["properties"]["hostName"]
    require(matches(FRONT_DOOR_HOST, hostname) and endpoint["properties"]["enabledState"] == "Enabled"
            and not arm_entries(read(f"{profile_id}/customDomains", FRONT_DOOR)),
            "Front Door qualification uses one enabled default hostname")

    private = access == "private"
    route_name = "crab-immutable" if private else "crab-immutable-export"
    route = one_resource(arm_entries(read(f"{endpoint_id}/routes", FRONT_DOOR)), f"{endpoint_id}/routes/{route_name}",
                         "qualification endpoint needs one immutable route")
    deployed(route)
    props = route["properties"]
    require(props["enabledState"] == "Enabled" and props["linkToDefaultDomain"] == "Enabled"
            and props["httpsRedirect"] == "Disabled" and props["supportedProtocols"] == ["Https"]
            and props["forwardingProtocol"] == "HttpsOnly" and props["patternsToMatch"] == ["/*"]
            and props["originPath"] == f"/{container}" and not props.get("customDomains") and not props.get("ruleSets"),
            "Front Door route must use the approved HTTPS Blob mapping without aliases or rewrites")
    cache = props["cacheConfiguration"]
    require(cache["queryStringCachingBehavior"] == ("IgnoreQueryString" if private else "UseQueryString")
            and not cache.get("queryParameters") and cache["compressionSettings"]["isCompressionEnabled"] is False,
            "Front Door cache configuration differs from the approved query and representation policy")

    group_name = "crab-blob" if private else "crab-export-blob"
    group_id = f"{profile_id}/originGroups/{group_name}"
    require(props["originGroup"]["id"].lower() == group_id.lower(), "Front Door route points to a different origin group")
    group = resource(read(group_id, FRONT_DOOR), group_id)
    deployed(group)
    auth = group["properties"].get("authentication")
    if private:
        require(isinstance(auth, dict) and auth["type"] == "SystemAssignedIdentity"
                and auth["scope"] == "https://storage.azure.com/.default" and not auth.get("userAssignedIdentity"),
                "Front Door private origin requires the system-assigned Blob identity")
    else:
        require(not auth, "Front Door public export requires its anonymous Blob origin")
    origin = one_resource(arm_entries(read(f"{group_id}/origins", FRONT_DOOR)), f"{group_id}/origins/{group_name}",
                          "Front Door qualification needs a single Blob origin")
    deployed(origin)
    blob = origin["properties"]
    require(blob["hostName"] == blob["originHostHeader"] == host and blob["httpsPort"] == 443
            and blob["enforceCertificateNameCheck"] is True and blob["enabledState"] == "Enabled"
            and not blob.get("sharedPrivateLinkResource"), "Front Door must load the approved Blob origin over verified TLS")
    return {"profile": profile, "endpoint": endpoint, "route": route, "origin-group": group, "origin": origin}


def apim_policy(xml: str, hostname: str, blob_origin: str, edge_host: str, prefix: str, suffix: str) -> str:
    require("<!DOCTYPE" not in xml, "APIM viewer policy needs inline XML")
    try:
        keys = ElementTree.fromstring(xml).findall("inbound/validate-jwt/issuer-signing-keys/key")
        key_id = re.compile(r"[A-Za-z0-9_-]{1,63}")
        modulus = re.compile(r"[A-Za-z0-9_-]{342}")
        exponent = re.compile(r"[A-Za-z0-9_-]{1,16}")
        require(1 <= len(keys) <= 3 and len({key.get("id") for key in keys}) == len(keys)
                and all(matches(key_id, key.get("id")) and matches(modulus, key.get("n"))
                        and matches(exponent, key.get("e")) for key in keys), "APIM viewer requires distinct RSA signing keys")
        key_xml = "\n".join('<key id="{id}" n="{n}" e="{e}" />'.format(**key.attrib) for key in keys)
        rendered = (AZURE_TEMPLATES / "viewer-policy.xml").read_text(encoding="utf-8")
        for marker, value in {
            "VIEWER_HOST": hostname, "BLOB_ORIGIN": blob_origin, "EDGE_HOST": edge_host, "SIGNING_KEYS": key_xml,
            "REPOSITORY_PREFIX_BASE64": base64.b64encode(prefix.encode()).decode(),
            "PROTECTED_VIEW_SUFFIX_BASE64": base64.b64encode(suffix.encode()).decode(),
        }.items():
            rendered = rendered.replace(f"__{marker}__", value)
        canonical = ElementTree.canonicalize(xml, strip_text=True)
        require(canonical == ElementTree.canonicalize(rendered, strip_text=True),
                "APIM viewer policy differs from the reviewed route and credential checks")
    except ElementTree.ParseError:
        raise CdnEvidenceError("Azure returned malformed APIM policy XML") from None
    return canonical


def gateway_policy(read: Callable, group_id: str, base: str, fixture: dict, edge_host: str) -> tuple[dict, set[str]]:
    parsed = urlsplit(base)
    name = parsed.hostname.removesuffix(".azure-api.net")
    require(matches(re.compile(r"[a-z][a-z0-9-]{0,48}[a-z0-9]"), name)
            and base == f"https://{name}.azure-api.net/crab", "private Front Door fixture needs a dedicated default APIM endpoint")
    gateway_id = f"{group_id}/providers/Microsoft.ApiManagement/service/{name}"
    gateway = resource(read(gateway_id, APIM), gateway_id)
    deployed(gateway)
    props = gateway["properties"]
    require(gateway["sku"]["name"] == "Standard" and gateway["sku"]["capacity"] == 1
            and props["gatewayUrl"].rstrip("/") == f"https://{parsed.hostname}" and props["virtualNetworkType"] == "None"
            and props["publicNetworkAccess"] == "Enabled" and not props.get("additionalLocations")
            and all(entry["hostName"] == parsed.hostname for entry in props.get("hostnameConfigurations", []) if entry.get("type") == "Proxy"),
            "private Front Door requires a dedicated classic Standard APIM gateway")
    sockets = {str(ipaddress.IPv4Address(address)) for address in props["publicIPAddresses"]}
    require(bool(sockets), "APIM must report its exclusive public socket addresses")
    api_id = f"{gateway_id}/apis/crab-viewer"
    api = one_resource(arm_entries(read(f"{gateway_id}/apis", APIM)), api_id, "APIM fixture needs a single viewer API")
    api_props = api["properties"]
    require((api_props["path"], api_props["protocols"], api_props["subscriptionRequired"]) == ("crab", ["https"], False),
            "APIM viewer API must enforce its dedicated HTTPS route")
    operation_id = f"{api_id}/operations/get-crab-object"
    operation = one_resource(arm_entries(read(f"{api_id}/operations", APIM)), operation_id, "APIM viewer API needs a single GET operation")
    require((operation["properties"]["method"], operation["properties"]["urlTemplate"]) == ("GET", "/*")
            and not arm_entries(read(f"{operation_id}/policies", APIM)), "APIM operation must inherit the reviewed viewer policy")
    policy_id = f"{api_id}/policies/policy"
    policy = resource(read(policy_id, APIM, format="rawxml"), policy_id)["properties"]
    require(policy["format"] == "rawxml", "APIM must return the active inline viewer policy")
    reviewed = apim_policy(policy["value"], parsed.hostname, fixture["native_origin"], edge_host,
                           fixture["fixture_prefix"], fixture["view_suffix"])
    # ARM gateway responses contain mutable operational fields unrelated to the route.
    return {"gateway-id": gateway_id, "gateway-region": gateway["location"], "sockets": sorted(sockets),
            "service-settings": props.get("customProperties", {}), "hostnames": props.get("hostnameConfigurations", []),
            "api": api, "operation": operation, "policy": reviewed}, sockets


def waf_condition(variable: str, operator: str, values: list[str], *, selector: str = "", negate: bool = False) -> tuple:
    return (variable, operator, selector, negate, tuple(sorted(values)))


def observed_condition(value: dict[str, Any]) -> tuple:
    require(not value.get("transforms"), "Front Door WAF must check the untransformed request")
    values = value.get("matchValue", [])
    if value["matchVariable"] == "SocketAddr":
        addresses = [ipaddress.IPv4Interface(address) for address in values]
        require(all(address.network.prefixlen == 32 for address in addresses), "Front Door WAF must match exact APIM socket addresses")
        values = [str(address.ip) for address in addresses]
    return waf_condition(value["matchVariable"], value["operator"], values,
                         selector=value.get("selector") or "", negate=value.get("negateCondition", False))


def public_waf_rules(hostname: str, prefix: str) -> dict[int, tuple]:
    shape = decode((AZURE_TEMPLATES / "public-export/immutable-routes.json").read_bytes())
    request = [waf_condition("RequestMethod", "Equal", ["GET"]), waf_condition("RequestHeader", "Equal", [hostname], selector="Host")]
    rules = {1: ("Block", [waf_condition("QueryString", "GreaterThan", ["0"])]),
             100: ("Block", [waf_condition("RequestUri", "Any", [])])}
    for priority, family in ((10, "git"), (11, "lfs")):
        patterns = [shape[family]["shape"].replace("__REPOSITORY__", prefix.replace(".", r"\.")), *shape[family]["fanout"]]
        rules[priority] = ("Allow", sorted(request + [waf_condition("RequestUri", "Regex", [pattern]) for pattern in patterns]))
    return rules


def viewer_policy(read: Callable, group_id: str, profile_id: str, endpoint: dict, fixture: dict, access: str) -> dict:
    name = "crab-viewer" if access == "private" else "crab-public-export"
    association = one_resource(arm_entries(read(f"{profile_id}/securityPolicies", FRONT_DOOR)),
                               f"{profile_id}/securityPolicies/{name}", "Front Door requires one WAF security policy")
    deployed(association)
    settings = association["properties"]["parameters"]
    domains = settings["associations"]
    require(settings["type"] == "WebApplicationFirewall" and len(domains) == 1 and domains[0]["patternsToMatch"] == ["/*"]
            and [domain["id"].lower() for domain in domains[0]["domains"]] == [endpoint["id"].lower()]
            and all(domain.get("isActive", True) for domain in domains[0]["domains"]),
            "Front Door WAF must cover all paths on the approved default hostname")
    waf_id = settings["wafPolicy"]["id"]
    require(matches(re.compile(rf"{re.escape(group_id)}/providers/Microsoft.Network/frontDoorWebApplicationFirewallPolicies/[A-Za-z0-9]+",
                               re.IGNORECASE), waf_id), "Front Door WAF reference left the approved resource group")
    waf = resource(read(waf_id, WAF), waf_id)
    deployed(waf)
    props = waf["properties"]
    policy = props["policySettings"]
    require(waf["sku"]["name"] == "Premium_AzureFrontDoor" and policy["enabledState"] == "Enabled"
            and policy["mode"] == "Prevention" and not props.get("managedRules", {}).get("managedRuleSets")
            and not policy.get("customBlockResponseBody") and policy.get("customBlockResponseStatusCode", 403) == 403,
            "Front Door WAF must enforce the approved custom rules in prevention mode")
    hostname = endpoint["properties"]["hostName"]
    if access == "private":
        gateway, sockets = gateway_policy(read, group_id, fixture["base_url"].rstrip("/"), fixture, hostname)
        expected = {1: ("Block", [waf_condition("SocketAddr", "IPMatch", list(sockets), negate=True)])}
    else:
        require(fixture["base_url"].rstrip("/") == f"https://{hostname}", "public Front Door endpoint differs from the approved fixture")
        gateway = None
        expected = public_waf_rules(hostname, fixture["fixture_prefix"])
    rules = props["customRules"]["rules"]
    require(len(rules) == len(expected) and all(rule["ruleType"] == "MatchRule" and rule.get("enabledState", "Enabled") == "Enabled"
                                              for rule in rules), "Front Door WAF rules must be complete and enforced")
    observed = {rule["priority"]: (rule["action"], sorted(observed_condition(condition) for condition in rule["matchConditions"])) for rule in rules}
    require(observed == expected, "Front Door WAF differs from the approved socket or immutable route policy")
    return {"association": association, "waf": waf, "gateway": gateway}


def blob_policy(read: Callable, account_id: str, container_id: str, route: dict, origin: str, access: str, fixture: dict) -> dict:
    account = resource(read(account_id, BLOB), account_id)
    deployed(account)
    props = account["properties"]
    public = access == "public"
    require(props["primaryEndpoints"]["blob"].rstrip("/") == f"https://{urlsplit(origin).hostname}"
            and props["isHnsEnabled"] is True and props["supportsHttpsTrafficOnly"] is True and props["minimumTlsVersion"] == "TLS1_2"
            and props["allowSharedKeyAccess"] is False and props["allowBlobPublicAccess"] is public,
            "Azure Blob account sharing or transport settings differ from the approved origin")
    container = resource(read(container_id, BLOB), container_id)
    require(container["properties"].get("publicAccess", "None") == ("Blob" if public else "None"),
            "Azure Blob container access differs from the approved disclosure")
    if public:
        tags = account["tags"]
        require(tags.get("CrabPublicExport") == "true" and tags.get("CrabSourceOrigin") == fixture["native_origin"]
                and matches(SHA256, tags.get("CrabApprovedInventorySha256")), "Azure export origin must identify its approved source and inventory")
        service_id = f"{account_id}/blobServices/default"
        settings = resource(read(service_id, BLOB), service_id)["properties"]
        require(settings.get("isVersioningEnabled", False) is False
                and settings["deleteRetentionPolicy"]["enabled"] is False and settings["containerDeleteRetentionPolicy"]["enabled"] is False,
                "Azure public export needs disabled versioning and soft delete")
        authority = settings
    else:
        identity = route["profile"]["identity"]
        principal = identity["principalId"]
        require(identity["type"] == "SystemAssigned" and matches(SUBSCRIPTION, principal), "Front Door needs its dedicated managed identity")
        assignments = arm_entries(read(f"{container_id}/providers/Microsoft.Authorization/roleAssignments", RBAC,
                                       **{"$filter": f"principalId eq {principal}"}))
        require(len(assignments) == 1, "Front Door identity must have only its container reader assignment")
        assignment = assignments[0]["properties"]
        require(assignment["principalId"].lower() == principal.lower() and assignment["principalType"] == "ServicePrincipal"
                and assignment["scope"].lower() == container_id.lower() and assignment["roleDefinitionId"].lower().endswith(f"/roledefinitions/{BLOB_READER}")
                and not assignment.get("condition"), "Front Door identity must read only the approved Blob container")
        authority = assignment
    # Bind access controls without including fields that change during data writes.
    return {"account-id": account_id, "account-region": account["location"], "settings": props, "tags": account.get("tags", {}),
            "container-id": container_id, "container-access": container["properties"].get("publicAccess", "None"), "authority": authority}


def collect(fixture: dict, subscription: str, group: str, profile: str) -> PolicyObservation:
    edge, access, _, _, _ = scope_parts(fixture["scope"])
    require(edge == "front-door" and matches(SUBSCRIPTION, subscription) and matches(RESOURCE_GROUP, group)
            and group not in {".", ".."} and matches(PROFILE, profile) and len(profile) <= 63,
            "Front Door qualification needs an approved Azure subscription, resource group and profile")
    authorization = bearer("az", "account", "get-access-token", "--subscription", subscription, "--resource", "https://management.azure.com/",
                           "--query", "accessToken", "--output", "tsv", "--only-show-errors")

    def read(path: str, version: str, **options: str) -> dict[str, Any]:
        return arm_document(path, version, authorization, **options)

    resource_group = f"/subscriptions/{subscription}/resourceGroups/{group}"
    profile_id = f"{resource_group}/providers/Microsoft.Cdn/profiles/{profile}"
    native = fixture["export_origin"] if access == "public" else fixture["native_origin"]
    container = origin_bucket(native, edge)
    host = urlsplit(native).hostname
    account_id = f"{resource_group}/providers/Microsoft.Storage/storageAccounts/{host.split('.')[0]}"
    container_id = f"{account_id}/blobServices/default/containers/{container}"
    route = front_door_route(read, profile_id, profile, host, container, access)
    viewer = viewer_policy(read, resource_group, profile_id, route["endpoint"], fixture, access)
    blob = blob_policy(read, account_id, container_id, route, native, access, fixture)
    current = resource(read(route["route"]["id"], FRONT_DOOR), route["route"]["id"])
    require(current["properties"] == route["route"]["properties"], "Front Door route changed during attestation")
    keys = []
    export_binding = {}
    private_binding = None
    if access == "private":
        policy = ElementTree.fromstring(viewer["gateway"]["policy"])
        keys = sorted(key.attrib["id"] for key in policy.findall("inbound/validate-jwt/issuer-signing-keys/key"))
        location = blob["settings"].get("primaryLocation")
        require(matches(re.compile(r"[a-z0-9]{1,40}"), location), "Azure native Blob account needs an observed primary location")
        private_binding = PrivateBinding(current["id"].lower(), blob["container-id"].lower(), location)
    else:
        inventory = blob["tags"]["CrabApprovedInventorySha256"]
        source_container_id = fixture["source_placement"]["resource"]
        source_account_id = source_container_id.split("/blobservices/", 1)[0]
        source = resource(read(source_account_id, BLOB), source_account_id)
        source_container = resource(read(source_container_id, BLOB), source_container_id)
        settings = source["properties"]
        require(settings["primaryEndpoints"]["blob"].rstrip("/") == fixture["native_origin"].rsplit("/", 1)[0],
                "Azure live source does not match its public cloud endpoint")
        export_binding["public-export"] = public_export_binding(
            fixture, route["route"]["id"], inventory,
            {"resource": source_container["id"].lower(), "location": settings["primaryLocation"]},
            {"resource": blob["container-id"].lower(), "location": blob["settings"]["primaryLocation"]},
        )
    components = {name: deployment_component_digest(value) for name, value in {
        "front-door-route": route, "viewer-policy": viewer, "blob-access": blob, "viewer-key-ids": keys, **export_binding,
    }.items()}
    return PolicyObservation(components, private_binding)


def main() -> int:
    parser = deployment_parser(__doc__)
    parser.add_argument("--subscription", required=True)
    parser.add_argument("--resource-group", required=True)
    parser.add_argument("--profile", required=True)
    args = parser.parse_args()
    return collect_deployment_policy(args, "Front Door", lambda fixture: collect(fixture, args.subscription, args.resource_group, args.profile))


if __name__ == "__main__":
    raise SystemExit(main())
