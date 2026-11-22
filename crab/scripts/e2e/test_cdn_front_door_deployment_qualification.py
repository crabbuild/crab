"""Exercise Front Door deployment evidence using representative Azure ARM responses."""

import base64
import unittest
from unittest.mock import patch

import collect_front_door_deployment_policy as COLLECTOR
from cdn_qualification import (
    deployment_component_digest, deployment_component_names, deployment_policy_digest, public_export_binding,
)
from test_cdn_transport_qualification import REPO, fixture


SUBSCRIPTION = "01234567-0123-0123-0123-0123456789ab"
PRINCIPAL = "12345678-1234-1234-1234-123456789012"
GROUP = "crab-qualification"
PROFILE = "crab-fixture"
GROUP_ID = f"/subscriptions/{SUBSCRIPTION}/resourceGroups/{GROUP}"
PROFILE_ID = f"{GROUP_ID}/providers/Microsoft.Cdn/profiles/{PROFILE}"
HOST = "fixture-0123.z01.azurefd.net"
GATEWAY = "crab-gateway.azure-api.net"
SOCKET = "203.0.113.5"
TOKEN = "Bearer fixture-secret"


class FrontDoorDeploymentQualificationTests(unittest.TestCase):
    def configure(self, access: str = "private", *, protected: bool = False) -> None:
        self.fixture = fixture("front-door", access)
        self.fixture["base_url"] = f"https://{GATEWAY}/crab" if access == "private" else f"https://{HOST}"
        self.fixture["view_suffix"] = f"/acl-views/v2/{'a' * 64}/7-{'b' * 64}" if protected else ""
        self.documents: dict[tuple[str, str], dict] = {}
        private = access == "private"
        self.profile = self.add(PROFILE_ID, COLLECTOR.FRONT_DOOR, {}, sku={"name": "Premium_AzureFrontDoor"})
        endpoint_id = f"{PROFILE_ID}/afdEndpoints/{PROFILE}"
        self.endpoint = self.add(endpoint_id, COLLECTOR.FRONT_DOOR, {"enabledState": "Enabled", "hostName": HOST})
        self.list(f"{PROFILE_ID}/afdEndpoints", COLLECTOR.FRONT_DOOR, self.endpoint)
        self.list(f"{PROFILE_ID}/customDomains", COLLECTOR.FRONT_DOOR)
        group_name = "crab-blob" if private else "crab-export-blob"
        group_id = f"{PROFILE_ID}/originGroups/{group_name}"
        route_name = "crab-immutable" if private else "crab-immutable-export"
        self.route = self.add(f"{endpoint_id}/routes/{route_name}", COLLECTOR.FRONT_DOOR, {
            "enabledState": "Enabled", "linkToDefaultDomain": "Enabled", "httpsRedirect": "Disabled",
            "supportedProtocols": ["Https"], "forwardingProtocol": "HttpsOnly", "patternsToMatch": ["/*"],
            "originPath": "/container", "originGroup": {"id": group_id}, "cacheConfiguration": {
                "queryStringCachingBehavior": "IgnoreQueryString" if private else "UseQueryString",
                "compressionSettings": {"isCompressionEnabled": False},
            },
        })
        self.routes = self.list(f"{endpoint_id}/routes", COLLECTOR.FRONT_DOOR, self.route)
        self.origin_group = self.add(group_id, COLLECTOR.FRONT_DOOR, {})
        if private:
            self.origin_group["properties"]["authentication"] = {"type": "SystemAssignedIdentity", "scope": "https://storage.azure.com/.default"}
        origin = "fixtureacct" if private else "exportacct"
        blob_host = f"{origin}.blob.core.windows.net"
        self.origin = self.add(f"{group_id}/origins/{group_name}", COLLECTOR.FRONT_DOOR, {
            "hostName": blob_host, "originHostHeader": blob_host, "httpsPort": 443,
            "enforceCertificateNameCheck": True, "enabledState": "Enabled",
        })
        self.list(f"{group_id}/origins", COLLECTOR.FRONT_DOOR, self.origin)
        self.prepare_viewer(endpoint_id, access)

        account_id = f"{GROUP_ID}/providers/Microsoft.Storage/storageAccounts/{origin}"
        container_id = f"{account_id}/blobServices/default/containers/container"
        account = self.add(account_id, COLLECTOR.BLOB, {
            "primaryEndpoints": {"blob": f"https://{blob_host}/"}, "primaryLocation": "westus2", "isHnsEnabled": True,
            "supportsHttpsTrafficOnly": True, "minimumTlsVersion": "TLS1_2", "allowSharedKeyAccess": False,
            "allowBlobPublicAccess": not private,
        }, location="westus2")
        self.add(container_id, COLLECTOR.BLOB, {"publicAccess": "None" if private else "Blob"})
        if private:
            self.profile["identity"] = {"type": "SystemAssigned", "principalId": PRINCIPAL}
            self.assignment = {"principalId": PRINCIPAL, "principalType": "ServicePrincipal", "scope": container_id,
                               "roleDefinitionId": f"/subscriptions/{SUBSCRIPTION}/providers/Microsoft.Authorization/roleDefinitions/{COLLECTOR.BLOB_READER}"}
            self.list(f"{container_id}/providers/Microsoft.Authorization/roleAssignments", COLLECTOR.RBAC, {"properties": self.assignment})
        else:
            live_container = self.fixture["source_placement"]["resource"]
            live_account = live_container.split("/blobservices/", 1)[0]
            self.live_source = self.add(live_account, COLLECTOR.BLOB, {
                "primaryEndpoints": {"blob": "https://fixtureacct.blob.core.windows.net/"}, "primaryLocation": "eastus",
            })
            self.add(live_container, COLLECTOR.BLOB, {"publicAccess": "None"})
            self.export_placement = {"resource": container_id.lower(), "location": "westus2"}
            account["tags"] = {"CrabPublicExport": "true", "CrabSourceOrigin": self.fixture["native_origin"],
                               "CrabApprovedInventorySha256": "d" * 64}
            self.add(f"{account_id}/blobServices/default", COLLECTOR.BLOB, {
                "isVersioningEnabled": False, "deleteRetentionPolicy": {"enabled": False}, "containerDeleteRetentionPolicy": {"enabled": False},
            })

    def add(self, resource_id, version, properties, **fields):
        item = {"id": resource_id, "properties": {"provisioningState": "Succeeded", **properties}, **fields}
        self.documents[(resource_id, version)] = item
        return item

    def list(self, resource_id, version, *entries):
        value = list(entries)
        self.documents[(resource_id, version)] = {"value": value}
        return value

    def prepare_viewer(self, endpoint_id: str, access: str) -> None:
        private = access == "private"
        association_name = "crab-viewer" if private else "crab-public-export"
        waf_id = f"{GROUP_ID}/providers/Microsoft.Network/frontDoorWebApplicationFirewallPolicies/CrabFixture"
        security = self.add(f"{PROFILE_ID}/securityPolicies/{association_name}", COLLECTOR.FRONT_DOOR, {"parameters": {
            "type": "WebApplicationFirewall", "wafPolicy": {"id": waf_id},
            "associations": [{"domains": [{"id": endpoint_id, "isActive": True}], "patternsToMatch": ["/*"]}],
        }})
        self.list(f"{PROFILE_ID}/securityPolicies", COLLECTOR.FRONT_DOOR, security)
        if private:
            self.prepare_gateway()
            expected = {1: ("Block", [("SocketAddr", "IPMatch", "", True, (f"{SOCKET}/32",))])}
        else:
            expected = COLLECTOR.public_waf_rules(HOST, REPO)
        self.waf_rules = [{
            "priority": priority, "ruleType": "MatchRule", "action": action, "enabledState": "Enabled",
            "matchConditions": [{"matchVariable": var, "operator": op, "selector": sel or None, "negateCondition": neg,
                                 "matchValue": list(values), "transforms": []} for var, op, sel, neg, values in rules],
        } for priority, (action, rules) in expected.items()]
        self.add(waf_id, COLLECTOR.WAF, {"policySettings": {"enabledState": "Enabled", "mode": "Prevention"},
                                       "customRules": {"rules": self.waf_rules}}, sku={"name": "Premium_AzureFrontDoor"})

    def prepare_gateway(self) -> None:
        gateway_id = f"{GROUP_ID}/providers/Microsoft.ApiManagement/service/crab-gateway"
        self.add(gateway_id, COLLECTOR.APIM, {
            "gatewayUrl": f"https://{GATEWAY}", "virtualNetworkType": "None", "publicIPAddresses": [SOCKET],
            "publicNetworkAccess": "Enabled", "hostnameConfigurations": [{"type": "Proxy", "hostName": GATEWAY}],
        }, sku={"name": "Standard", "capacity": 1}, location="westus2")
        api_id = f"{gateway_id}/apis/crab-viewer"
        api = self.add(api_id, COLLECTOR.APIM, {"path": "crab", "protocols": ["https"], "subscriptionRequired": False})
        self.list(f"{gateway_id}/apis", COLLECTOR.APIM, api)
        operation_id = f"{api_id}/operations/get-crab-object"
        operation = self.add(operation_id, COLLECTOR.APIM, {"method": "GET", "urlTemplate": "/*"})
        self.list(f"{api_id}/operations", COLLECTOR.APIM, operation)
        self.overrides = self.list(f"{operation_id}/policies", COLLECTOR.APIM)
        policy = (COLLECTOR.AZURE_TEMPLATES / "viewer-policy.xml").read_text(encoding="utf-8")
        for marker, value in {
            "VIEWER_HOST": GATEWAY, "BLOB_ORIGIN": self.fixture["native_origin"], "EDGE_HOST": HOST,
            "SIGNING_KEYS": f'<key id="public-test" n="{"a" * 342}" e="AQAB" />',
            "REPOSITORY_PREFIX_BASE64": base64.b64encode(REPO.encode()).decode(),
            "PROTECTED_VIEW_SUFFIX_BASE64": base64.b64encode(self.fixture["view_suffix"].encode()).decode(),
        }.items():
            policy = policy.replace(f"__{marker}__", value)
        self.policy = self.add(f"{api_id}/policies/policy", COLLECTOR.APIM, {"format": "rawxml", "value": policy})

    def read(self, path, version, authorization, **options):
        self.assertEqual(authorization, TOKEN)
        if path.endswith("roleAssignments"):
            self.assertEqual(options, {"$filter": f"principalId eq {PRINCIPAL}"})
        elif path.endswith("/policies/policy"):
            self.assertEqual(options, {"format": "rawxml"})
        else:
            self.assertEqual(options, {})
        return self.documents[(path, version)]

    def collect(self):
        with (patch.object(COLLECTOR, "bearer", return_value=TOKEN), patch.object(COLLECTOR, "arm_document", side_effect=self.read)):
            return COLLECTOR.collect(self.fixture, SUBSCRIPTION, GROUP, PROFILE)

    def test_reference_profiles_attest_the_approved_configuration(self):
        for access, protected in (("private", False), ("private", True), ("public", False)):
            with self.subTest(access=access, protected=protected):
                self.configure(access, protected=protected)
                observation = self.collect()
                components = observation.bound_components(self.fixture)
                self.assertEqual(components.keys(), deployment_component_names("front-door", access))
                if access == "public":
                    binding = public_export_binding(self.fixture, self.route["id"], "d" * 64,
                                                    self.fixture["source_placement"], self.export_placement)
                    self.assertEqual(components["public-export"], deployment_component_digest(binding))
                else:
                    binding = observation.private_binding.route(self.fixture)
                    self.assertEqual((binding["deployment"], binding["native_resource"], binding["native_location"]),
                                     (self.route["id"].lower(), self.assignment["scope"].lower(), "westus2"))
                    self.assertEqual(components["private-binding"], deployment_component_digest(binding))
                self.assertEqual(components["viewer-key-ids"], deployment_component_digest(["public-test"] if access == "private" else []))
                self.assertRegex(deployment_policy_digest("front-door", access, components), r"^[a-f0-9]{64}$")
                self.assertNotIn(TOKEN, str(components))

    def test_operation_policy_override_rejects_private_deployment(self):
        self.configure()
        self.overrides.append({"id": "policy"})
        with self.assertRaisesRegex(ValueError, "must inherit the reviewed viewer policy"):
            self.collect()

    def test_rotation_wait_distinguishes_deployment_progress_from_failure(self):
        for provisioning, deployment in (("Updating", "NotStarted"), ("Succeeded", "InProgress")):
            with self.subTest(provisioning=provisioning, deployment=deployment):
                self.configure()
                self.route["properties"].update(provisioningState=provisioning, deploymentStatus=deployment)
                with self.assertRaisesRegex(COLLECTOR.PolicyDeploymentPending, "deployment is in progress"):
                    self.collect()
        self.configure()
        self.route["properties"]["deploymentStatus"] = "Failed"
        with self.assertRaisesRegex(ValueError, "must be provisioned and deployed") as failed:
            self.collect()
        self.assertNotIsInstance(failed.exception, COLLECTOR.PolicyDeploymentPending)

    def test_private_waf_must_match_socket_address_without_widening(self):
        for field, value, error in (
            ("matchVariable", "RemoteAddr", "differs from the approved socket"),
            ("matchValue", ["203.0.113.0/24"], "must match exact APIM socket addresses"),
        ):
            self.configure()
            self.waf_rules[0]["matchConditions"][0][field] = value
            with self.assertRaisesRegex(ValueError, error):
                self.collect()

    def test_private_identity_must_be_scoped_to_the_container(self):
        self.configure()
        self.assignment["scope"] = GROUP_ID
        with self.assertRaisesRegex(ValueError, "must read only the approved Blob container"):
            self.collect()

    def test_apim_jwt_policy_with_expiry_disabled_rejects_attestation(self):
        self.configure()
        self.policy["properties"]["value"] = self.policy["properties"]["value"].replace('require-expiration-time="true"', 'require-expiration-time="false"')
        with self.assertRaisesRegex(ValueError, "APIM viewer policy differs"):
            self.collect()

    def test_public_route_requires_immutable_waf_and_a_single_route(self):
        for section, error in (("waf", "differs from the approved socket or immutable route policy"),
                               ("route", "qualification endpoint needs one immutable route")):
            self.configure("public")
            if section == "waf":
                self.waf_rules[1]["matchConditions"].pop()
            else:
                self.routes.append(self.route)
            with self.assertRaisesRegex(ValueError, error):
                self.collect()
