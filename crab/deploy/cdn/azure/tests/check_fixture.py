"""Check the compiled ARM wiring and the gateway's canonical path rules."""

import json
import re
import sys
import unittest
from itertools import count
from pathlib import Path
from xml.etree import ElementTree


def route_pattern(pattern: str) -> re.Pattern:
    """Translate named groups, backreferences and anchors shared with .NET."""
    kinds = count()
    pattern = re.sub(r"\(\?<kind>", lambda _: f"(?<kind{next(kinds)}>", pattern)
    pattern = re.sub(r"\(\?<(\w+)>", r"(?P<\1>", pattern)
    pattern = re.sub(r"\\k<(\w+)>", r"(?P=\1)", pattern)
    return re.compile(pattern.replace(r"\z", r"\Z"))


class AzureFixtureTests(unittest.TestCase):
    template: dict

    @classmethod
    def setUpClass(cls):
        cls.resources = cls.template["resources"]
        text = next(
            value
            for value in cls.template["variables"].values()
            if isinstance(value, str) and value.startswith("<policies>")
        )
        replacements = {
            "SIGNING_KEYS": '<key id="test-key" n="fixture" e="AQAB" />',
            "VIEWER_HOST": "crab-gateway.azure-api.net",
            "BLOB_ORIGIN": "https://crabstorage.blob.core.windows.net/test",
            "EDGE_HOST": "crab-edge.example.invalid",
            "REPOSITORY_PREFIX_BASE64": "b3JnL3JlcG8=",
            "PROTECTED_VIEW_SUFFIX_BASE64": "",
        }
        for key, value in replacements.items():
            text = text.replace(f"__{key}__", value)
        cls.policy = ElementTree.fromstring(text)

    def test_private_origin_requires_entra_and_scopes_front_door_identity(self):
        storage = self.resources["storage"]["properties"]
        self.assertFalse(storage["allowBlobPublicAccess"])
        self.assertFalse(storage["allowSharedKeyAccess"])
        self.assertTrue(storage["isHnsEnabled"])
        self.assertEqual(self.resources["container"]["properties"]["publicAccess"], "None")

        grant = self.resources["fillRole"]
        self.assertIn("Microsoft.Storage/storageAccounts/blobServices/containers", grant["scope"])
        self.assertIn("parameters('containerName')", grant["scope"])
        self.assertIn("2a2b9908-6ea1-4ae2-8e65-a410df84e7d1", grant["properties"]["roleDefinitionId"])
        origin_group = self.resources["originGroup"]
        self.assertIn("fillRole", origin_group["dependsOn"])
        self.assertEqual(
            origin_group["properties"]["authentication"],
            {"type": "SystemAssignedIdentity", "scope": "https://storage.azure.com/.default"},
        )

    def test_cached_endpoint_requires_the_dedicated_gateway_socket(self):
        self.assertEqual(self.resources["gateway"]["sku"], {"name": "Standard", "capacity": 1})
        self.assertEqual(self.resources["gateway"]["properties"]["virtualNetworkType"], "None")
        props = self.resources["waf"]["properties"]
        self.assertEqual(props["policySettings"], {"enabledState": "Enabled", "mode": "Prevention"})
        rule, = props["customRules"]["rules"]
        self.assertEqual(rule["action"], "Block")
        self.assertEqual(
            rule["matchConditions"],
            [{
                "matchVariable": "SocketAddr",
                "operator": "IPMatch",
                "negateCondition": True,
                "matchValue": "[reference('gateway').publicIPAddresses]",
            }],
        )
        association, = self.resources["edgeSecurity"]["properties"]["parameters"]["associations"]
        self.assertEqual(association["patternsToMatch"], ["/*"])
        self.assertIn("endpoint", association["domains"][0]["id"])

        route = self.resources["route"]
        self.assertIn("edgeSecurity", route["dependsOn"])
        self.assertIn("apiPolicy", route["dependsOn"])
        self.assertFalse(self.template["parameters"]["enableViewerRoute"]["defaultValue"])
        self.assertEqual(route["properties"]["enabledState"], "[if(parameters('enableViewerRoute'), 'Enabled', 'Disabled')]")
        self.assertEqual(route["properties"]["supportedProtocols"], ["Https"])
        self.assertEqual(route["properties"]["forwardingProtocol"], "HttpsOnly")
        self.assertFalse(route["properties"]["cacheConfiguration"]["compressionSettings"]["isCompressionEnabled"])

    def test_gateway_validates_viewer_before_forwarding(self):
        inbound = self.policy.find("inbound")
        elements = [element.tag for element in inbound]
        jwt = inbound.find("validate-jwt")
        self.assertEqual(jwt.attrib["header-name"], "Authorization")
        self.assertEqual(jwt.attrib["require-scheme"], "Bearer")
        self.assertEqual(jwt.attrib["require-expiration-time"], "true")
        self.assertEqual(jwt.attrib["require-signed-tokens"], "true")
        self.assertEqual(jwt.attrib["clock-skew"], "0")
        self.assertEqual(jwt.findtext("audiences/audience"), "https://crab-gateway.azure-api.net/crab")
        self.assertEqual(jwt.findtext("issuers/issuer"), jwt.findtext("audiences/audience"))
        self.assertEqual(
            jwt.findtext("required-claims/claim[@name='origin']/value"),
            "https://crabstorage.blob.core.windows.net/test",
        )
        self.assertLess(elements.index("validate-jwt"), elements.index("rewrite-uri"))
        self.assertIsNotNone(inbound.find("set-header[@name='Authorization'][@exists-action='delete']"))
        self.assertEqual(inbound.find("rewrite-uri").attrib["copy-unmatched-params"], "false")
        self.assertEqual(inbound.find("set-backend-service").attrib["base-url"], "https://crab-edge.example.invalid")
        forward = self.policy.find("backend/forward-request")
        self.assertEqual(forward.attrib["buffer-response"], "false")
        self.assertEqual(forward.attrib["follow-redirects"], "false")
        self.assertEqual(
            self.policy.findtext("outbound/set-header[@name='Cache-Control']/value"),
            "private, no-store",
        )
        operations = [
            resource["properties"]
            for resource in self.resources.values()
            if resource["type"] == "Microsoft.ApiManagement/service/apis/operations"
        ]
        operation, = operations
        self.assertEqual((operation["method"], operation["urlTemplate"]), ("GET", "/*"))

    def test_gateway_routes_only_canonical_immutable_object_keys(self):
        expression = self.policy.find("inbound/set-variable[@name='crab-object-path']").attrib["value"]
        patterns = [
            route_pattern(pattern)
            for pattern in re.findall(r'Regex.Match\(key, @"([^"]+)"\)', expression)
        ]
        self.assertEqual(len(patterns), 3)
        git, lfs = patterns[:2]
        self.assertIn('suffix.Length != 0 && xet.Success ? xet.Groups["kind"].Value : ""', expression)
        digest = "abcd" + "3" * 60
        for key, expected in {
            f"v2/capsules/ab/{digest}": "v2/capsules",
            f"v2/checkpoints/ab/{digest}": "v2/checkpoints",
            f"v2/catalogs/ab/{digest}": "v2/catalogs",
            f"v2/pack-layers/ab/{digest}": "v2/pack-layers",
            f"v2/history/ab/{digest}": "v2/history",
            f"v2/history/source-roots/ab/{digest}": "v2/history",
            f"lfs/objects/ab/cd/{digest}": "lfs/objects",
            "v2/root": None,
            f"v2/capsules/aa/{digest}": None,
            f"v2/capsules/%61b/{digest}": None,
            f"v2/capsules/ab/{digest.upper()}": None,
            f"v2/capsules/source-roots/ab/{digest}": None,
            f"v2/capsules/ab/{digest}/other": None,
            f"lfs/objects/ab/cc/{digest}": None,
            f".crab/xorbs/ab/{digest}": None,
        }.items():
            with self.subTest(key=key):
                kinds = [
                    value
                    for pattern in (git, lfs)
                    if (match := pattern.fullmatch(key))
                    for name, value in match.groupdict().items()
                    if name.startswith("kind") and value is not None
                ]
                self.assertEqual(kinds, [] if expected is None else [expected])
        self.assertIn('viewer.Claims["prefix"][0] != repository + "/" + kind + "/"', expression)
        self.assertIn('viewer.Subject != repository', expression)

    def test_protected_view_routes_only_its_own_xet_keys(self):
        expression = self.policy.find("inbound/set-variable[@name='crab-object-path']").attrib["value"]
        suffix_pattern = route_pattern(re.search(r'Regex.IsMatch\(suffix, @"([^"]+)"\)', expression)[1])
        xet = route_pattern(re.search(r'var xet = Regex.Match\(key, @"([^"]+)"\)', expression)[1])
        suffix = f"/acl-views/v2/{'a' * 64}/7-{'b' * 64}"
        self.assertTrue(suffix_pattern.fullmatch(suffix))
        for invalid in [suffix.replace("v2", "v1"), suffix.replace("/7-", "/07-"), suffix + "/extra"]:
            self.assertIsNone(suffix_pattern.fullmatch(invalid))

        self.assertIn('var repository = source + suffix;', expression)
        self.assertIn('path.StartsWith("/crab/" + repository + "/", StringComparison.Ordinal)', expression)
        self.assertIn('viewer.Claims["prefix"][0] != repository + "/" + kind + "/"', expression)
        # Check the regexes embedded in the compiled policy. The live APIM gate
        # still exercises JWT validation and the gateway's URL handling.
        digest = "abcd" + "3" * 60
        pinned = f"/crab/org/repo{suffix}/"
        for family in ("xorbs", "shards", "capsule-chunk-pages"):
            key = f".crab/{family}/ab/{digest}"
            for uri, expected in [
                (pinned + key, True),
                (pinned.replace("/7-", "/8-") + key, False),
                (pinned.replace("a" * 64, "c" * 64) + key, False),
                ("/crab/org/repo/" + key, False),
                ("/crab/" + key, False),
                (pinned + key.replace("/ab/", "/aa/"), False),
                (pinned + key.replace("/ab/", "/%61b/"), False),
                (pinned + key + "/tail", False),
            ]:
                with self.subTest(uri=uri):
                    self.assertEqual(uri.startswith(pinned) and bool(xet.fullmatch(uri[len(pinned):])), expected)


if __name__ == "__main__":
    AzureFixtureTests.template = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
    unittest.main(argv=[sys.argv[0]])
