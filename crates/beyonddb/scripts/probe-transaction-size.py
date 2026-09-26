#!/usr/bin/env python3
"""Compare transaction-size semantics on an explicitly selected DynamoDB endpoint."""

import argparse
import base64
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    endpoint = parser.add_mutually_exclusive_group(required=True)
    endpoint.add_argument("--endpoint-url", help="Local/reference endpoint; uses dummy credentials")
    endpoint.add_argument("--profile", help="AWS profile for a temporary cloud reference table")
    parser.add_argument("--region", default="us-east-1")
    args = parser.parse_args()
    command = ["aws", "--region", args.region, "--output", "json", "--cli-binary-format", "base64"]
    environment = dict(os.environ, AWS_PAGER="")
    if args.endpoint_url:
        command += ["--endpoint-url", args.endpoint_url]
        environment.update(AWS_ACCESS_KEY_ID="localtest", AWS_SECRET_ACCESS_KEY="localtest")
        environment.pop("AWS_SESSION_TOKEN", None)
    else:
        command += ["--profile", args.profile]
        for name in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"):
            environment.pop(name, None)
    table = "BeyondDbSizeProbe-" + uuid.uuid4().hex[:16]
    keys = [{"id": {"S": str(i)}} for i in range(12)]
    payload = "x" * (380 * 1024)
    items = [{**key, "payload": {"S": payload}} for key in keys]
    print(json.dumps({"table": table, "items": 12, "payload_bytes_per_item": len(payload)}), flush=True)

    with tempfile.TemporaryDirectory(prefix="beyonddb-size-probe-") as directory:
        request = Path(directory) / "request.json"

        def call(action, body, *, check=True):
            request.write_text(json.dumps(body))
            result = subprocess.run(command + ["dynamodb", *action.split(), "--cli-input-json", "file://" + str(request)],
                                    env=environment, capture_output=True, text=True, timeout=180)
            if check and result.returncode:
                raise RuntimeError(result.stderr.strip())
            return result

        try:
            call("create-table", {"TableName": table, "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
                                  "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}], "BillingMode": "PAY_PER_REQUEST"})
            call("wait table-exists", {"TableName": table})
            for kind in ("update_same", "delete", "check", "put_shrink", "update_shrink", "put_large"):
                seeded = json.loads(call("batch-write-item", {"RequestItems": {table: [{"PutRequest": {"Item": item}} for item in items]}}).stdout)
                if seeded.get("UnprocessedItems"):
                    raise RuntimeError("Reference seed was throttled; no size verdict is valid")
                operations = []
                for key, item in zip(keys, items):
                    base = {"TableName": table, "Key": key}
                    if kind == "update_same":
                        operation = {"Update": {**base, "UpdateExpression": "SET flag = :v", "ExpressionAttributeValues": {":v": {"BOOL": True}}}}
                    elif kind == "update_shrink":
                        operation = {"Update": {**base, "UpdateExpression": "REMOVE payload"}}
                    elif kind == "delete":
                        operation = {"Delete": base}
                    elif kind == "check":
                        operation = {"ConditionCheck": {**base, "ConditionExpression": "attribute_exists(id)"}}
                    else:
                        operation = {"Put": {"TableName": table, "Item": key if kind == "put_shrink" else item}}
                    operations.append(operation)
                result = call("transact-write-items", {"TransactItems": operations}, check=False)
                accepted = result.returncode == 0
                # Verify every affected key without printing item payloads. Rejected
                # transactions must preserve the complete seed too.
                for key, original in zip(keys, items):
                    actual = json.loads(call("get-item", {"TableName": table, "Key": key, "ConsistentRead": True}).stdout or "{}").get("Item")
                    expected = original
                    if accepted:
                        if kind == "delete":
                            expected = None
                        elif kind in ("put_shrink", "update_shrink"):
                            expected = key
                        elif kind == "update_same":
                            expected = {**original, "flag": {"BOOL": True}}
                    if actual != expected:
                        raise RuntimeError(f"{kind}: stored result disagrees with transaction outcome")
                print(json.dumps({"case": kind, "accepted": accepted, "error": result.stderr.strip() or None,
                                  "all_item_results_verified": True}), flush=True)
            for kind, count, value in (
                ("read_binary", 10, {"B": base64.b64encode(bytes([0xa5]) * (380 * 1024)).decode()}),
                ("read_escaped", 4, {"S": "\0" * (380 * 1024)}),
            ):
                read_items = [{**key, "payload": value} for key in keys[:count]]
                seeded = json.loads(call("batch-write-item", {"RequestItems": {table: [{"PutRequest": {"Item": item}} for item in read_items]}}).stdout)
                if seeded.get("UnprocessedItems"):
                    raise RuntimeError("Reference seed was throttled; no size verdict is valid")
                reads = [{"Get": {"TableName": table, "Key": key}} for key in reversed(keys[:count])]
                result = call("transact-get-items", {"TransactItems": reads}, check=False)
                if result.returncode == 0:
                    actual = [response.get("Item") for response in json.loads(result.stdout)["Responses"]]
                    if actual != list(reversed(read_items)):
                        raise RuntimeError(f"{kind}: read result differs from seeded images")
                print(json.dumps({"case": kind, "accepted": result.returncode == 0,
                                  "error": result.stderr.strip() or None,
                                  "all_item_results_verified": result.returncode == 0}), flush=True)
        finally:
            result = call("delete-table", {"TableName": table}, check=False)
            if result.returncode and "ResourceNotFoundException" not in result.stderr:
                raise RuntimeError(f"Cleanup failed for {table}: {result.stderr.strip()}")
            call("wait table-not-exists", {"TableName": table})
            print(json.dumps({"deleted_table": table}), flush=True)


if __name__ == "__main__":
    main()
