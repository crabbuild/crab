import base64
from contextlib import closing, nullcontext
import hashlib
import json
from pathlib import Path
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch
from urllib.parse import parse_qs, urlsplit
import xml.etree.ElementTree as ET

import publish_export as publisher
from public_inventory import SCHEMA


class ExportTest(unittest.TestCase):
    def setUp(self):
        scratch = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.data = b"approved bytes"
        digest = hashlib.sha256(self.data).hexdigest()
        path = f"lfs/objects/{digest[:2]}/{digest[2:4]}/{digest}"
        self.source_origin = "https://crabfixturelive.blob.core.windows.net/repo"
        self.source_account = ("/subscriptions/55555555-4444-3333-2222-111111111111/resourcegroups/live/"
                               "providers/microsoft.storage/storageaccounts/crabfixturelive")
        self.source_container = self.source_account + "/blobservices/default/containers/repo"
        raw = json.dumps({
            "schema": SCHEMA, "source_origin": self.source_origin,
            "source_placement": {"resource": self.source_container, "location": "westus2"},
            "repository_prefix": "org/repo",
            "objects": [{"path": path, "size": len(self.data), "sha256": digest}],
        }).encode()
        self.inventory_hash = hashlib.sha256(raw).hexdigest()
        self.options = SimpleNamespace(
            inventory=scratch / "inventory.json", scratch_directory=scratch,
            operator_configuration=scratch / "operator", source_configuration=scratch / "reader",
            writer_configuration=scratch / "writer", deployment="test-export",
            resource_group="crab-fixture", subscription="11111111-2222-3333-4444-555555555555",
        )
        self.options.inventory.write_bytes(raw)
        root = (f"/subscriptions/{self.options.subscription}/"
                f"resourceGroups/{self.options.resource_group}/providers")
        self.account = f"{root}/Microsoft.Storage/storageAccounts/crabfixtureexport"
        self.service = self.account + "/blobServices/default"
        self.container = self.service + "/containers/export"
        edge = f"{root}/Microsoft.Cdn/profiles/fixture"
        self.origin_group = edge + "/originGroups/crab-export-blob"
        self.origin = self.origin_group + "/origins/crab-export-blob"
        self.route = edge + "/afdEndpoints/fixture/routes/crab-immutable-export"
        self.endpoint = edge + "/afdEndpoints/fixture"
        self.host = "crabfixtureexport.blob.core.windows.net"
        self.source_location = "westus2"
        self.edge_host = self.host
        self.viewer_host = "fixture.azurefd.net"
        self.key = f"org/repo/{path}"
        self.source_data = self.data
        self.export_data = None
        self.metadata = {}
        self.writes = 0
        self.object_operations = 0
        self.blocks = {}
        self.commits = 0
        self.racer = None
        self.failed_block = None
        self.mutate_source = False
        self.token_requests = 0
        self.enterContext(patch.object(publisher, "az", self.az))
        self.connection = Mock()
        self.connection.request.side_effect = self.request
        self.connection.getresponse.side_effect = lambda: nullcontext(self.response)
        self.connect = self.enterContext(patch.object(publisher, "HTTPSConnection", return_value=self.connection))

    def az(self, configuration, args):
        if args == ["cloud", "show"]:
            return {"name": "AzureCloud"}
        if args[:2] == ["account", "get-access-token"]:
            self.assertEqual(configuration, self.options.writer_configuration)
            self.assertEqual(args[2:], ["--resource", publisher.STORAGE_AUDIENCE])
            self.token_requests += 1
            return {"accessToken": "fixture-writer-token", "tokenType": "Bearer",
                    "expires_on": int(time.time()) + 3600}
        if args[:3] == ["deployment", "group", "show"]:
            self.assertEqual(configuration, self.options.operator_configuration)
            outputs = {
                "liveSourceOrigin": self.source_origin, "exportOrigin": f"https://{self.host}/export",
                "sourceRepositoryPrefix": "org/repo", "inventorySha256": self.inventory_hash,
                "publicDeliveryEnabled": False, "exportContainerId": self.container,
                "edgeOriginId": self.origin, "edgeRouteId": self.route,
                "baseUrl": "https://fixture.azurefd.net",
            }
            return {"properties": {
                "provisioningState": "Succeeded",
                "outputs": {key: {"value": value} for key, value in outputs.items()},
            }}
        if args[:2] == ["resource", "show"]:
            self.assertEqual(configuration, self.options.operator_configuration)
            target = args[args.index("--ids") + 1]
            properties = {
                self.account: {
                    "allowBlobPublicAccess": False, "allowSharedKeyAccess": False, "isHnsEnabled": True,
                    "primaryLocation": "eastus",
                    "primaryEndpoints": {"blob": f"https://{self.host}/"},
                },
                self.source_account: {"primaryLocation": self.source_location, "primaryEndpoints": {
                    "blob": "https://crabfixturelive.blob.core.windows.net/",
                }},
                self.source_container: {"publicAccess": "None"},
                self.service: {
                    "deleteRetentionPolicy": {"enabled": False},
                    "containerDeleteRetentionPolicy": {"enabled": False},
                },
                self.container: {"publicAccess": "None"},
                self.origin: {"hostName": self.edge_host, "originHostHeader": self.edge_host},
                self.route: {"enabledState": "Disabled", "originPath": "/export",
                             "originGroup": {"id": self.origin_group}},
                self.endpoint: {"hostName": self.viewer_host},
            }[target]
            return {"id": target, "properties": properties, "tags": {
                "CrabPublicExport": "true", "CrabApprovedInventorySha256": self.inventory_hash,
                "CrabSourceOrigin": self.source_origin,
            }}

        self.object_operations += 1
        self.assertEqual(args[:2], ["storage", "blob"])
        self.assertEqual(args[args.index("--name") + 1], self.key)
        self.assertEqual(args[args.index("--auth-mode") + 1], "login")
        source = args[args.index("--account-name") + 1] == "crabfixturelive"
        self.assertEqual(configuration, self.options.source_configuration if source else
                         self.options.writer_configuration)
        self.assertEqual(args[args.index("--container-name") + 1], "repo" if source else "export")
        body = self.source_data if source else self.export_data
        if args[2] == "show":
            return {"metadata": self.metadata, "properties": {
                "contentLength": len(body), "etag": '"fixture"', "blobType": "BlockBlob",
                "contentSettings": {"contentType": publisher.CONTENT_TYPE,
                                    "cacheControl": publisher.CACHE_CONTROL},
            }}
        path = Path(args[args.index("--file") + 1])
        self.assertEqual(args[2], "download")
        self.assertEqual(args[args.index("--if-match") + 1], '"fixture"')
        path.write_bytes(body)
        if source:
            self.source_spool = path
        return {}

    def reply(self, status, error_code=""):
        self.response = Mock(status=status)
        self.response.read.return_value = b""
        self.response.getheader.return_value = error_code

    def request(self, method, target, body, headers):
        self.object_operations += 1
        self.assertEqual(headers["Authorization"], "Bearer fixture-writer-token")
        self.assertEqual(headers["x-ms-version"], "2023-11-03")
        url = urlsplit(target)
        self.assertEqual(url.path, f"/export/{self.key}")
        query = parse_qs(url.query)
        if method == "HEAD":
            if self.mutate_source:
                self.source_spool.write_bytes(b"X" * len(self.data))
            if self.export_data is None:
                return self.reply(404, "BlobNotFound")
            return self.reply(200)
        self.assertEqual(method, "PUT")
        if query.get("comp") == ["block"]:
            self.assertNotIn("If-None-Match", headers)
            block = body.read()
            self.assertEqual(int(headers["Content-Length"]), len(block))
            checksum = base64.b64encode(hashlib.md5(block, usedforsecurity=False).digest()).decode()
            self.assertEqual(headers["Content-MD5"], checksum)
            block_id, = query["blockid"]
            decoded = base64.b64decode(block_id).decode("ascii")
            self.assertEqual(len(decoded), 37)
            if int(decoded[-5:]) == self.failed_block:
                return self.reply(503, "ServerBusy")
            self.assertNotIn(block_id, self.blocks)
            self.blocks[block_id] = block
            return self.reply(201)
        self.assertEqual(headers["If-None-Match"], "*")
        self.assertEqual(headers["x-ms-blob-cache-control"], publisher.CACHE_CONTROL)
        if query == {"comp": ["blocklist"]}:
            self.commits += 1
            self.assertEqual(headers["x-ms-blob-content-type"], publisher.CONTENT_TYPE)
            xml = ET.fromstring(body)
            self.assertEqual(xml.tag, "BlockList")
            self.assertTrue(all(entry.tag == "Uncommitted" for entry in xml))
            block_ids = [entry.text for entry in xml]
            decoded_ids = [base64.b64decode(block_id).decode() for block_id in block_ids]
            self.assertEqual(len({value[:-5] for value in decoded_ids}), 1)
            self.assertEqual([int(value[-5:]) for value in decoded_ids], list(range(1, len(block_ids) + 1)))
            body = b"".join(self.blocks[block_id] for block_id in block_ids)
            if self.racer is not None:
                self.export_data = self.racer
        else:
            self.assertEqual(query, {})
            self.assertEqual(headers["x-ms-blob-type"], "BlockBlob")
            self.assertEqual(headers["Content-Type"], publisher.CONTENT_TYPE)
            checksum = base64.b64encode(hashlib.md5(body, usedforsecurity=False).digest()).decode()
            self.assertEqual(headers["Content-MD5"], checksum)
        if self.export_data is not None:
            return self.reply(412, "ConditionNotMet")
        self.export_data = body
        self.blocks.clear()
        self.writes += 1
        return self.reply(201)

    def test_create_only_and_retry_against_verified_existing_blob(self):
        receipt = publisher.publish(self.options)
        self.assertEqual(publisher.publish(self.options), receipt)
        self.assertEqual(receipt["inventory_sha256"], self.inventory_hash)
        self.assertEqual(receipt["base_url"], "https://fixture.azurefd.net")
        self.assertEqual(receipt["deployment"], self.route)
        self.assertEqual(receipt["source_placement"]["resource"], self.source_container)
        self.assertEqual(receipt["export_placement"], {"resource": self.container.lower(), "location": "eastus"})
        self.assertEqual(self.export_data, self.data)
        self.assertEqual(self.writes, 1)
        self.assertEqual(self.connect.call_args.args, (self.host,))
        self.metadata = {"unapproved": "fixture"}
        with self.assertRaisesRegex(publisher.ExportError, "representation differs"):
            publisher.publish(self.options)
        self.assertEqual(self.writes, 1)

    def test_rejects_changed_body_at_source_and_existing_destination(self):
        replacement = b"x" * len(self.data)
        self.source_data = replacement
        with self.assertRaisesRegex(publisher.ExportError, "digest mismatch"):
            publisher.publish(self.options)
        self.source_data = self.data
        self.export_data = replacement
        with self.assertRaisesRegex(publisher.ExportError, "digest mismatch"):
            publisher.publish(self.options)
        self.assertEqual(self.writes, 0)

    def test_multipart_retry_isolates_blocks_and_commits_only_the_verified_attempt(self):
        with patch.multiple(publisher, SINGLE_PUT_BYTES=8, DEFAULT_BLOCK_BYTES=5):
            self.failed_block = 2
            with self.assertRaisesRegex(publisher.ExportError, "stage block failed"):
                publisher.publish(self.options)
            self.assertIsNone(self.export_data)
            self.assertEqual((len(self.blocks), self.commits), (1, 0))
            self.failed_block = None
            receipt = publisher.publish(self.options)
            self.assertEqual(publisher.publish(self.options), receipt)
        self.assertEqual(self.export_data, self.data)
        self.assertEqual((self.writes, self.commits, len(self.blocks)), (1, 1, 0))

    def test_multipart_competing_blob_must_pass_conditional_readback(self):
        self.racer = b"x" * len(self.data)
        with patch.multiple(publisher, SINGLE_PUT_BYTES=8, DEFAULT_BLOCK_BYTES=5):
            receipt = self.options.scratch_directory / "receipt.json"
            with self.assertRaisesRegex(publisher.ExportError, "inventory digest mismatch"):
                publisher.write_publication(receipt, lambda: publisher.publish(self.options))
        self.assertEqual((self.writes, self.commits), (0, 1))
        self.assertEqual(self.export_data, self.racer)
        self.assertFalse(receipt.exists())

    def test_multipart_verifies_spooled_bytes_before_committing(self):
        self.mutate_source = True
        with patch.multiple(publisher, SINGLE_PUT_BYTES=8, DEFAULT_BLOCK_BYTES=5):
            with self.assertRaisesRegex(publisher.ExportError, "source spool differs"):
                publisher.publish(self.options)
        self.assertEqual(self.commits, 0)
        self.assertIsNone(self.export_data)

    def test_writer_renews_a_nearly_expired_storage_token_between_requests(self):
        with closing(publisher.BlobWriter(self.options.writer_configuration, f"https://{self.host}/export")) as writer:
            for _ in range(2):
                self.assertEqual(writer.request("HEAD", f"/export/{self.key}"), (404, "BlobNotFound"))
            self.assertEqual(self.token_requests, 1)
            writer.expires_on = int(time.time()) + 5
            self.assertEqual(writer.request("HEAD", f"/export/{self.key}"), (404, "BlobNotFound"))
        self.assertEqual(self.token_requests, 2)

    def test_origin_binding_is_checked_before_any_blob_io(self):
        self.edge_host = "crabfixturelive.blob.core.windows.net"
        with self.assertRaisesRegex(publisher.ExportError, "does not serve the staged export"):
            publisher.publish(self.options)
        self.assertEqual(self.object_operations, 0)

    def test_viewer_hostname_mismatch_is_checked_before_any_blob_io(self):
        self.viewer_host = "other.azurefd.net"
        with self.assertRaisesRegex(publisher.ExportError, "does not serve the staged export"):
            publisher.publish(self.options)
        self.assertEqual(self.object_operations, 0)

    def test_export_on_the_source_account_is_checked_before_any_blob_io(self):
        self.host = "crabfixturelive.blob.core.windows.net"
        with self.assertRaisesRegex(publisher.ExportError, "separate export bucket or Azure storage account"):
            publisher.publish(self.options)
        self.assertEqual(self.object_operations, 0)

    def test_changed_native_primary_location_is_checked_before_any_blob_io(self):
        self.source_location = "westus3"
        with self.assertRaisesRegex(publisher.ExportError, "approved placement"):
            publisher.publish(self.options)
        self.assertEqual(self.object_operations, 0)


if __name__ == "__main__":
    unittest.main()
