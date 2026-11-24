"""Check the compiled public ARM template and its exact immutable path policy."""

import json
from pathlib import Path
import re
import sys


def check_template(template):
    resources = template["resources"]
    params = template["parameters"]
    variables = template["variables"]
    storage = resources["exportStorage"]["properties"]
    blobs = resources["blobs"]["properties"]
    container = resources["container"]["properties"]
    route = resources["route"]
    origin = resources["origin"]["properties"]
    security = resources["edgeSecurity"]["properties"]["parameters"]
    waf = resources["waf"]["properties"]

    assert params["enablePublicDelivery"]["defaultValue"] is False
    assert storage["isHnsEnabled"] is True
    assert storage["allowSharedKeyAccess"] is False
    assert storage["allowBlobPublicAccess"] == "[variables('active')]"
    assert not blobs.get("isVersioningEnabled", False)
    assert blobs["deleteRetentionPolicy"]["enabled"] is False
    assert blobs["containerDeleteRetentionPolicy"]["enabled"] is False
    assert container["publicAccess"] == "[if(variables('active'), 'Blob', 'None')]"
    assert route["properties"]["enabledState"] == "[if(variables('active'), 'Enabled', 'Disabled')]"
    assert "parameters('enablePublicDelivery')" in variables["active"]
    assert "parameters('approvedInventorySha256')" in variables["active"]
    assert "variables('validOrigin')" in variables["active"]
    assert "parameters('storageAccountName')" in variables["originChecks"][-1]
    assert route["properties"]["originPath"] == "[format('/{0}', parameters('containerName'))]"
    assert origin["hostName"] == origin["originHostHeader"] == "[variables('blobHost')]"
    assert "edgeSecurity" in route["dependsOn"]
    assert route["properties"]["linkToDefaultDomain"] == "Enabled"
    assert route["properties"]["patternsToMatch"] == ["/*"]
    assert route["properties"]["supportedProtocols"] == ["Https"]
    assert route["properties"]["forwardingProtocol"] == "HttpsOnly"
    assert route["properties"]["cacheConfiguration"]["compressionSettings"]["isCompressionEnabled"] is False
    assert route["properties"]["cacheConfiguration"]["queryStringCachingBehavior"] == "UseQueryString"
    assert len(security["associations"]) == 1
    association = security["associations"][0]
    assert association["patternsToMatch"] == ["/*"]
    assert len(association["domains"]) == 1
    assert "Microsoft.Cdn/profiles/afdEndpoints" in association["domains"][0]["id"]
    assert waf["policySettings"] == {"enabledState": "Enabled", "mode": "Prevention"}
    denied_query, git, lfs, denied_other = waf["customRules"]["rules"]
    assert [rule["priority"] for rule in (denied_query, git, lfs, denied_other)] == [1, 10, 11, 100]
    assert denied_query["action"] == "Block"
    assert denied_query["matchConditions"] == [{
        "matchVariable": "QueryString", "operator": "GreaterThan", "matchValue": ["0"]
    }]
    assert denied_other["action"] == "Block"
    assert denied_other["matchConditions"][0]["operator"] == "Any"
    for family, rule in (("git", git), ("lfs", lfs)):
        assert rule["action"] == "Allow"
        guard = rule["matchConditions"]
        assert f"variables('shape').{family}.shape" in guard
        assert f"variables('shape').{family}.fanout" in guard
        assert "'RequestMethod', 'operator', 'Equal', 'matchValue', createArray('GET')" in guard
        assert "'RequestHeader', 'selector', 'Host'" in guard and "reference('endpoint').hostName" in guard
    function = template["functions"][0]["members"]["allowImmutable"]["output"]["value"]
    assert "'RequestUri', 'operator', 'Regex'" in function
    assert "concat(parameters('conditions')" in function
    assert "map(parameters('fanout')" in function
    # Resolve the static lookup emitted by Bicep's loadJsonContent.
    shape_ref = re.fullmatch(r"\[variables\('([^']+)'\)\]", variables["shape"])
    assert shape_ref
    return variables[shape_ref[1]], params["repositoryPrefix"]["maxLength"]


def accepts(policy, family, prefix, path):
    def matches(pattern):
        return re.search(pattern.replace(r"\z", r"\Z"), path) is not None

    shape = policy[family]["shape"].replace("__REPOSITORY__", prefix.replace(".", r"\."))
    return matches(shape) and all(matches(rule) for rule in policy[family]["fanout"])


def check_paths(policy, limit):
    prefix = "org/repo.v2"
    for digit in "0123456789abcdef":
        digest = digit * 64
        other = ("0" if digit != "0" else "1") + digit
        assert accepts(policy, "git", prefix, f"/{prefix}/v2/history/source-roots/{digit * 2}/{digest}")
        assert accepts(policy, "lfs", prefix, f"/{prefix}/lfs/objects/{digit * 2}/{digit * 2}/{digest}")
        for mismatch in (other, other[::-1]):
            assert not accepts(policy, "git", prefix, f"/{prefix}/v2/capsules/{mismatch}/{digest}")
            for fanout in (f"{mismatch}/{digit * 2}", f"{digit * 2}/{mismatch}"):
                assert not accepts(policy, "lfs", prefix, f"/{prefix}/lfs/objects/{fanout}/{digest}")

    key = f"/{prefix}/v2/checkpoints/aa/{'a' * 64}"
    for path in (
        f"/{prefix}/v2/root",
        f"/{prefix}/.crab/xorbs/aa/{'a' * 64}",
        key.replace("repo.v2", "repo-v2"),
        key.replace("repo.v2", "repo.v20"),
        key.replace("/aa/", "/%61a/"),
        key.replace("/v2/", "/other/v2/"),
        key + "?snapshot=2020-01-01",
        key + "/sibling",
        key + "\n",
    ):
        assert not any(accepts(policy, family, prefix, path) for family in policy), path

    # Front Door accepts 256 characters per regex value and 10 conditions per rule.
    for entry in policy.values():
        most_escaped = r"\." * (limit - 2) + "ab"
        assert len(entry["shape"].replace("__REPOSITORY__", most_escaped)) <= 256
        assert all(len(rule) <= 256 for rule in entry["fanout"])
        assert len(entry["fanout"]) + 3 <= 10
    assert len(policy) <= 5  # Regex-using rules per policy.


if __name__ == "__main__":
    template = json.loads(Path(sys.argv[1]).read_text())
    check_paths(*check_template(template))
