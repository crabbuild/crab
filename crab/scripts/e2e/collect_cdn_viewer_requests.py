#!/usr/bin/env python3
"""Join a timed Crab read to its provider's viewer access logs."""

from __future__ import annotations

import argparse
import hashlib
import ipaddress
import json
import re
import sys
import time
from collections.abc import Iterable, Iterator
from datetime import datetime, timedelta, timezone
from pathlib import Path
from tempfile import TemporaryDirectory
from urllib.parse import SplitResult, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from cdn_qualification import (  # noqa: E402
    CDN_PROVENANCE_FIELDS, GIT_FETCH_RECORD_LIMIT, GIT_OID, MOUNT_XORB_RANGE_READ, MOUNT_XORB_READ_FIELDS, PROVIDER_FOR_EDGE,
    SDK_HYDRATION_READ_FIELDS, SDK_WORKTREE_VIEWER_FILES, SDK_XORB_RANGE_READ, VIEWER_REQUEST_FILES, cli_payload_bytes, decode, fields,
    is_direct_desktop_git_scope, is_direct_mount_xorb_scope, is_direct_sdk_git_scope, is_direct_service_scope,
    is_direct_xorb_read_scope, matches,
    read_git_record, require, scope_parts, validate_desktop_git_read, validate_git_fetch, validate_sdk_git_record, validate_service_read,
    verify_git_traffic, verify_sparse_xorb_traffic, verify_viewer_requests,
)
from collect_cdn_fixture_cleanup import bearer  # noqa: E402
from cdn_viewer_issuer import issuer_settings  # noqa: E402
from collect_azure_access_logs import azure_log_archives  # noqa: E402
from collect_cloud_cdn_deployment_policy import PROJECT, RESOURCE  # noqa: E402
from collect_front_door_deployment_policy import FRONT_DOOR, FRONT_DOOR_HOST, SUBSCRIPTION, arm_document  # noqa: E402
from collect_cloudfront_access_logs import download_logs  # noqa: E402
from run_cdn_git_workload import read_target_fingerprints  # noqa: E402
from run_cdn_transport_checks import (  # noqa: E402
    approved_fixture, deployment_target, fixture_base, fixture_object_key, https_response, origin_bucket,
)


LOG_FIELDS = {b"x-edge-request-id", b"x-host-header", b"cs-method", b"cs-uri-stem", b"cs-protocol",
              b"sc-status", b"sc-bytes", b"x-edge-response-result-type",
              b"sc-content-len", b"sc-range-start", b"sc-range-end"}
CF_CACHE_STATUS = {b"Hit": "hit", b"Miss": "miss"}
AZURE_CACHE_STATUS = {"HIT": "hit", "REMOTE_HIT": "hit", "MISS": "miss"}
GCS_VIEWER_AGENT = re.compile(r"crab-cdn-qualification/[0-9a-f]{32}")
GCS_ORIGIN_AGENT = "Cloud-CDN-Google"
MAX_LINE = 64 * 1024
JSON_LOG_LIMIT = 32 * 1024 * 1024
GCP_LOG_PAGES = 20
GCP_LOG_PAGE_SIZE = 200
GCP_LOG_ENDPOINT = urlsplit("https://logging.googleapis.com")
S3_LOG_PREFIX = re.compile(rb'\S+ (?P<bucket>\S+) \[(?P<time>[^\]]+)\] \S+ \S+ (?P<id>\S+) REST\.GET\.OBJECT (?P<key>\S+) ')
S3_CLOUDFRONT_READ = re.compile(
    rb'"GET (?P<path>/[^" ]+) HTTP/[^" ]+" (?P<status>[1-5][0-9]{2}) \S+ (?P<bytes>-|[0-9]{1,20}) '
    rb'\S+ \S+ \S+ (?:"[^"]*"|-) "Amazon CloudFront" \S+ \S+ SigV4 \S+ AuthHeader (?P<host>\S+)(?: .*)?'
)
AZURE_BLOB_TIME = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,7})?Z")


def cloudfront_byte_field(value: bytes) -> int | None:
    if value == b"-":
        return None
    require(re.fullmatch(rb"[0-9]{1,20}", value) is not None, "invalid CloudFront log byte field")
    return int(value)


def cloudfront_logs(paths: list[Path], client: dict, fixture: dict) -> dict:
    expected = client["edge_responses"]
    base = fixture_base(fixture["base_url"])
    host = base.netloc.encode()
    base_path = (base.path.rstrip("/") + "/").encode()
    prefix = base_path + f"{fixture['fixture_prefix']}{fixture['view_suffix']}/".encode()
    records = {}
    for path in paths:
        names = []
        with path.open("rb") as source:
            while line := source.readline(MAX_LINE + 1):
                require(len(line) <= MAX_LINE, "CloudFront standard log line exceeds the size limit")
                line = line.rstrip(b"\r\n")
                if line.startswith(b"#Fields:"):
                    names = line.removeprefix(b"#Fields:").split()
                    require(set(names) >= LOG_FIELDS and len(names) == len(set(names)),
                            "CloudFront standard log is missing required or unique fields")
                elif line and not line.startswith(b"#"):
                    values = line.split(b"\t")
                    require(names and len(values) == len(names), "invalid CloudFront standard log record")
                    event = dict(zip(names, values, strict=True))
                    fingerprint = hashlib.sha256(event[b"x-edge-request-id"]).hexdigest()
                    if fingerprint not in expected:
                        continue
                    path_name = event[b"cs-uri-stem"]
                    require(event[b"x-host-header"] == host and event[b"cs-method"] == b"GET"
                            and event[b"cs-protocol"] == b"https" and path_name.startswith(prefix),
                            "CloudFront request was logged outside the approved viewer host or repository prefix")
                    status, size = event[b"sc-status"], event[b"sc-bytes"]
                    require(re.fullmatch(rb"[1-5][0-9]{2}", status) and re.fullmatch(rb"[0-9]{1,20}", size),
                            "CloudFront log has invalid HTTP status or response size")
                    start = cloudfront_byte_field(event[b"sc-range-start"])
                    end = cloudfront_byte_field(event[b"sc-range-end"])
                    record = {"status": int(status), "response_bytes": int(size),
                              "content_length": cloudfront_byte_field(event[b"sc-content-len"]),
                              "content_range": [start, end] if start is not None or end is not None else None,
                              "key_sha256": hashlib.sha256(path_name.removeprefix(base_path)).hexdigest(),
                              "cache": CF_CACHE_STATUS.get(event[b"x-edge-response-result-type"], "unknown")}
                    require(fingerprint not in records or records[fingerprint] == record,
                            "CloudFront exports disagree about one viewer request")
                    records[fingerprint] = record
            require(names, "CloudFront standard log needs a W3C field header")
    return records


def s3_cloudfront_reads(paths: list[Path], viewers: dict, fixture: dict, client: dict) -> dict:
    _, access, _, _, _ = scope_parts(fixture["scope"])
    origin = fixture["export_origin"] if access == "public" else fixture["native_origin"]
    bucket = origin_bucket(origin, "cloudfront").encode()
    host = re.compile(re.escape(bucket) + rb"\.s3(?:\.[a-z0-9-]+)?\.amazonaws\.com")
    viewer_keys = {row["key_sha256"] for row in viewers.values()}
    start, finish = (client[key] // 1000 for key in ("started_at_unix_ms", "finished_at_unix_ms"))
    records = {}
    for path in paths:
        with path.open("rb") as source:
            while line := source.readline(MAX_LINE + 1):
                require(len(line) <= MAX_LINE, "S3 server access log line exceeds the size limit")
                header = S3_LOG_PREFIX.match(line)
                if header is None or header["bucket"] != bucket:
                    continue
                key = header["key"].removeprefix(b"/")
                fingerprint = hashlib.sha256(key).hexdigest()
                if fingerprint not in viewer_keys:
                    continue
                read = S3_CLOUDFRONT_READ.fullmatch(line[header.end():].rstrip(b"\r\n"))
                if read is None:
                    continue  # A direct S3 reader can request the same immutable key.
                try:
                    at = int(datetime.strptime(header["time"].decode("ascii"), "%d/%b/%Y:%H:%M:%S %z").timestamp())
                except ValueError:
                    raise ValueError("invalid S3 server access log timestamp") from None
                if not start <= at <= finish:
                    continue
                require(read["path"] == b"/" + key and host.fullmatch(read["host"]) and header["id"] != b"-",
                        "CloudFront origin read was logged outside the approved S3 endpoint or object")
                size = 0 if read["bytes"] == b"-" else int(read["bytes"])
                row = {"key_sha256": fingerprint, "at_unix_s": at, "status": int(read["status"]), "response_body_bytes": size}
                request_id = hashlib.sha256(header["id"]).hexdigest()
                require(request_id not in records or records[request_id] == row,
                        "S3 access log exports disagree about one origin request")
                records[request_id] = row
    return records


def front_door_resources(fixture: dict) -> tuple[str, str, SplitResult]:
    deployment = deployment_target(fixture, "front-door")
    subscription, group, profile = (deployment[key] for key in ("subscription", "resource_group", "profile"))
    require(matches(SUBSCRIPTION, subscription), "Front Door viewer evidence needs the approved Azure subscription")
    group_id = f"/subscriptions/{subscription}/resourceGroups/{group}".lower()
    _, access, _, _, _ = scope_parts(fixture["scope"])
    native_origin = fixture["export_origin"] if access == "public" else fixture["native_origin"]
    origin = fixture_base(native_origin)
    account = origin.netloc.split(".", 1)[0]
    profile_id = f"{group_id}/providers/microsoft.cdn/profiles/{profile.lower()}"
    blob_id = f"{group_id}/providers/microsoft.storage/storageaccounts/{account}/blobservices/default"
    return profile_id, blob_id, origin


def front_door_logs(exports: Iterable[object], client: dict, fixture: dict, *, edge_location: bool = False) -> dict:
    expected = client["edge_responses"]
    resource, _, _ = front_door_resources(fixture)
    _, access, _, _, _ = scope_parts(fixture["scope"])
    base = fixture_base(fixture["base_url"])
    prefix = f"/{fixture['fixture_prefix']}{fixture['view_suffix']}/"
    records = {}
    for archive in exports:
        require(isinstance(archive, dict) and isinstance(archive.get("records"), list),
                "expected an Azure Monitor resource log archive with records")
        for row in archive["records"]:
            require(isinstance(row, dict), "invalid Azure Monitor resource log record")
            if str(row.get("category", "")).lower() != "frontdooraccesslog":
                continue
            props = row.get("properties")
            require(isinstance(props, dict), "invalid Front Door access log properties")
            reference = props.get("trackingReference")
            require(isinstance(reference, str) and bool(reference), "missing Front Door tracking reference")
            fingerprint = hashlib.sha256(reference.encode()).hexdigest()
            if fingerprint not in expected:
                continue
            from_client = props.get("isReceivedFromClient")
            if from_client is False or from_client == "false":
                # Front Door can also log the parent cache for the same request.
                continue
            require(from_client is True or from_client == "true", "missing Front Door viewer or origin-shield classification")
            endpoint, uri = props.get("endpoint"), props.get("requestUri")
            require(matches(FRONT_DOOR_HOST, endpoint) and isinstance(uri, str), "invalid Front Door request endpoint")
            url = urlsplit(uri)
            # Private APIM removes its /crab API prefix before forwarding to the
            # only endpoint in the attested Front Door profile.
            require(str(row.get("resourceId", "")).lower() == resource and (access == "private" or endpoint == base.netloc)
                    and url.scheme == "https" and url.netloc in (endpoint, f"{endpoint}:443") and not url.query and not url.fragment
                    and props.get("httpMethod") == "GET" and url.path.startswith(prefix),
                    "Front Door request was logged outside the approved profile or repository prefix")
            status = str(props.get("httpStatusCode"))
            size = str(props.get("responseBytes"))
            cache = props.get("cacheStatus")
            require(re.fullmatch(r"[1-5][0-9]{2}", status) and re.fullmatch(r"[0-9]{1,20}", size) and isinstance(cache, str),
                    "Front Door log has invalid HTTP status, response size or cache status")
            record = {"status": int(status), "response_bytes": int(size), "cache": AZURE_CACHE_STATUS.get(cache, "unknown"),
                      "key_sha256": hashlib.sha256(url.path[1:].encode()).hexdigest()}
            if edge_location:
                pop = props.get("pop")
                require(isinstance(pop, str) and re.fullmatch(r"[A-Za-z0-9-]{1,32}", pop),
                        "Front Door cache reuse requires a recorded viewer point of presence")
                record["pop_sha256"] = hashlib.sha256(pop.encode()).hexdigest()
                # REMOTE_HIT can describe delivery from a parent cache.
                record["cache"] = {"HIT": "hit", "MISS": "miss"}.get(cache, "unknown")
            require(fingerprint not in records or records[fingerprint] == record,
                    "Front Door exports disagree about one viewer request")
            records[fingerprint] = record
    return records


def azure_backend_networks(authority: dict) -> list[ipaddress.IPv4Network | ipaddress.IPv6Network]:
    tags = authority.get("values")
    require(authority.get("cloud") == "Public" and isinstance(tags, list), "expected Azure Public service tags")
    backend = [tag.get("properties") for tag in tags if isinstance(tag, dict) and tag.get("name") == "AzureFrontDoor.Backend"]
    require(len(backend) == 1 and isinstance(backend[0], dict), "Azure Public tags need the Front Door backend tag")
    prefixes = backend[0].get("addressPrefixes")
    require(isinstance(prefixes, list) and bool(prefixes) and all(isinstance(prefix, str) and "/" in prefix for prefix in prefixes),
            "missing Azure Front Door backend CIDRs")
    try:
        networks = [ipaddress.ip_network(prefix) for prefix in prefixes]
    except ValueError:
        raise ValueError("invalid Azure Front Door backend CIDRs") from None
    require(all(network.prefixlen > 0 for network in networks), "invalid Azure Front Door backend CIDRs")
    return networks


def azure_backend_ip(value: str) -> ipaddress.IPv4Address | ipaddress.IPv6Address:
    try:
        address = ipaddress.ip_address(value)
    except ValueError:
        host, _, port = value.rpartition(":")
        require(re.fullmatch(r"[0-9]{1,5}", port) and 0 < int(port) <= 65535, "invalid Azure Blob caller port")
        if host.startswith("[") and host.endswith("]"):
            host = host[1:-1]
        try:
            address = ipaddress.ip_address(host)
        except ValueError:
            raise ValueError("invalid Azure Blob caller address") from None
    if isinstance(address, ipaddress.IPv6Address) and address.ipv4_mapped is not None:
        return address.ipv4_mapped
    return address


def azure_blob_front_door_reads(exports: Iterable[object], authority: object, viewers: dict, fixture: dict, client: dict) -> dict:
    _, access, _, _, _ = scope_parts(fixture["scope"])
    profile, resource, origin = front_door_resources(fixture)
    root = origin.path.rstrip("/") + "/"
    require(isinstance(authority, dict), "expected Azure origin authority JSON")
    if access == "private":
        identity = authority.get("identity")
        require(str(authority.get("id", "")).lower() == profile and isinstance(identity, dict)
                and identity.get("type") == "SystemAssigned" and matches(SUBSCRIPTION, identity.get("principalId")),
                "Azure origin authority needs the approved Front Door profile's system-assigned identity")
        principal = identity["principalId"].lower()
    else:
        networks = azure_backend_networks(authority)
    keys = {row["key_sha256"] for row in viewers.values()}
    start, finish = (client[key] for key in ("started_at_unix_ms", "finished_at_unix_ms"))
    records = {}
    for archive in exports:
        require(isinstance(archive, dict) and isinstance(archive.get("records"), list),
                "expected an Azure Monitor Blob resource log archive with records")
        for row in archive["records"]:
            require(isinstance(row, dict), "invalid Azure Monitor Blob record")
            if (str(row.get("resourceId", "")).lower() != resource or row.get("category") != "StorageRead"
                    or row.get("operationName") != "GetBlob"):
                continue
            uri = row.get("uri")
            require(isinstance(uri, str), "Azure Blob origin log needs a request URL")
            url = urlsplit(uri)
            if not url.path.startswith(root):
                continue
            key = hashlib.sha256(url.path[len(root):].encode()).hexdigest()
            if key not in keys:
                continue
            identity = row.get("identity")
            auth = identity.get("type") if isinstance(identity, dict) else None
            if access == "private":
                requester = identity.get("requester") if auth == "OAuth" else None
                if not isinstance(requester, dict) or str(requester.get("objectId", "")).lower() != principal:
                    continue
            else:
                caller = row.get("callerIpAddress")
                if auth != "Anonymous" or not isinstance(caller, str):
                    continue
                address = azure_backend_ip(caller)
                if not any(address in net for net in networks):
                    continue
            timestamp = row.get("time")
            require(matches(AZURE_BLOB_TIME, timestamp), "invalid Azure Blob origin timestamp")
            try:
                received = datetime.fromisoformat(timestamp.removesuffix("Z") + "+00:00")
            except ValueError:
                raise ValueError("invalid Azure Blob origin timestamp") from None
            at = (received - datetime(1970, 1, 1, tzinfo=timezone.utc)) // timedelta(milliseconds=1)
            if not start <= at <= finish:
                continue
            require(url.scheme == "https" and url.netloc in (origin.netloc, f"{origin.netloc}:443")
                    and not url.query and not url.fragment,
                    "Front Door origin read was logged outside the approved Blob endpoint")
            status, request_id = str(row.get("statusCode")), row.get("correlationId")
            props = row.get("properties")
            require(isinstance(props, dict) and matches(SUBSCRIPTION, request_id)
                    and (re.fullmatch(r"[1-5][0-9]{2}", status) or status == "Unknown"),
                    "Azure Blob origin log has invalid status, properties or correlation ID")
            size = props.get("responseBodySize")
            if size is None and status not in ("200", "206"):
                size = 0
            require(type(size) is int and size >= 0, "invalid Azure Blob origin byte count")
            observation = {"key_sha256": key, "at_unix_ms": at, "status": 0 if status == "Unknown" else int(status),
                           "response_body_bytes": size}
            fingerprint = hashlib.sha256(request_id.lower().encode()).hexdigest()
            require(fingerprint not in records or records[fingerprint] == observation,
                    "Azure Blob log exports disagree about one origin request")
            records[fingerprint] = observation
    return records


def azure_monitor_tokens(profile: str) -> tuple[str, str]:
    subscription = profile.split("/")[2]
    token_args = ("az", "account", "get-access-token", "--subscription", subscription, "--query", "accessToken",
                  "--output", "tsv", "--only-show-errors")
    arm = bearer(*token_args, "--resource", "https://management.azure.com/")
    storage = bearer(*token_args, "--resource", "https://storage.azure.com/")
    return arm, storage


def azure_monitor_records(fixture: dict, client: dict, authority_path: Path | None) -> tuple[dict, dict]:
    profile, blob, _ = front_door_resources(fixture)
    arm, storage = azure_monitor_tokens(profile)
    authority = read_json_log(authority_path) if authority_path is not None else arm_document(profile, FRONT_DOOR, arm)
    viewers = front_door_logs(azure_log_archives(profile, "FrontDoorAccessLog", client, arm, storage, JSON_LOG_LIMIT),
                              client, fixture)
    origins = azure_blob_front_door_reads(azure_log_archives(blob, "StorageRead", client, arm, storage, JSON_LOG_LIMIT),
                                         authority, viewers, fixture, client)
    return viewers, origins


def read_json_log(path: Path):
    with path.open("rb") as source:
        raw = source.read(JSON_LOG_LIMIT + 1)
    require(len(raw) <= JSON_LOG_LIMIT, "provider log export exceeds the JSON size limit")
    return decode(raw)


def cloud_cdn_cache_status(request: dict, details: str) -> str:
    hit, lookup, revalidated = (request.get(key, False) for key in ("cacheHit", "cacheLookup", "cacheValidatedWithOriginServer"))
    require(all(type(value) is bool for value in (hit, lookup, revalidated)), "invalid Cloud CDN cache flags")
    if revalidated or details == "response_from_cache_validated":
        return "unknown"
    if hit or details in ("response_from_cache", "byte_range_caching"):
        return "hit"
    if lookup or details == "response_sent_by_backend":
        return "miss"
    return "unknown"


def cloud_cdn_forwarding_rule(entry: dict, project: str, rule: str) -> bool:
    resource = entry.get("resource")
    labels = resource.get("labels") if isinstance(resource, dict) else None
    return (isinstance(labels, dict) and resource.get("type") == "http_load_balancer"
            and entry.get("logName") == f"projects/{project}/logs/requests"
            and labels.get("project_id") == project and labels.get("forwarding_rule_name") == rule)


def gcp_logging_pages(fixture: dict, client: dict) -> Iterator[list]:
    target = deployment_target(fixture, "cloud-cdn")
    project, rule = target["project"], target["forwarding_rule"]
    require(matches(PROJECT, project) and matches(RESOURCE, rule),
            "Cloud Logging requires an approved GCP project and forwarding rule")
    epoch = datetime(1970, 1, 1, tzinfo=timezone.utc)
    # Include clock skew and cache-fill children initiated just after a viewer
    # response. The log parser still joins only the measured request IDs.
    try:
        start = epoch + timedelta(milliseconds=client["started_at_unix_ms"] - 60_000)
        finish = epoch + timedelta(milliseconds=client["finished_at_unix_ms"] + 60_000)
    except OverflowError:
        raise ValueError("invalid Cloud Logging measurement interval") from None
    filter_text = "\n".join((
        f'logName="projects/{project}/logs/requests"',
        'resource.type="http_load_balancer"',
        f'resource.labels.project_id="{project}"',
        f'resource.labels.forwarding_rule_name="{rule}"',
        f'timestamp >= "{start.isoformat(timespec="milliseconds")}"',
        f'timestamp <= "{finish.isoformat(timespec="milliseconds")}"',
        f'(httpRequest.userAgent =~ "^{GCS_VIEWER_AGENT.pattern}$" OR httpRequest.userAgent="{GCS_ORIGIN_AGENT}")',
    ))
    query = {"resourceNames": [f"projects/{project}"], "filter": filter_text,
             "pageSize": GCP_LOG_PAGE_SIZE, "orderBy": "timestamp desc"}
    headers = {"Authorization": bearer("gcloud", "auth", "print-access-token", "--quiet"),
               "Content-Type": "application/json", "Accept-Encoding": "identity"}
    remaining = JSON_LOG_LIMIT
    seen_tokens = set()
    for _ in range(GCP_LOG_PAGES):
        body = json.dumps(query).encode()
        with https_response(GCP_LOG_ENDPOINT, "POST", "/v2/entries:list", headers, "Cloud Logging", body=body) as response:
            require(response.status == 200, f"Cloud Logging expected HTTP 200, got {response.status}")
            raw = response.read(remaining + 1)
        remaining -= len(raw)
        require(remaining >= 0, "Cloud Logging results exceed the JSON size limit")
        try:
            page = decode(raw)
        except (ValueError, RecursionError):
            raise ValueError("invalid Cloud Logging result page") from None
        require(isinstance(page, dict) and page.keys() <= {"entries", "nextPageToken"}, "invalid Cloud Logging result page")
        entries, token = page.get("entries", []), page.get("nextPageToken", "")
        require(isinstance(entries, list) and len(entries) <= GCP_LOG_PAGE_SIZE
                and isinstance(token, str), "invalid Cloud Logging result page")
        yield entries
        if not token:
            return
        require(token not in seen_tokens, "Cloud Logging repeated a pagination token")
        seen_tokens.add(token)
        query["pageToken"] = token
    raise ValueError("Cloud Logging query exceeds the page limit")


def cloud_cdn_logs(exports: Iterable[object], client: dict, fixture: dict) -> dict:
    expected = client["edge_responses"]
    deployment = deployment_target(fixture, "cloud-cdn")
    project, rule = deployment["project"], deployment["forwarding_rule"]
    base = fixture_base(fixture["base_url"])
    base_path = base.path.rstrip("/") + "/"
    prefix = f"{base_path}{fixture['fixture_prefix']}{fixture['view_suffix']}/"
    records, inserts, origin_entries = {}, {}, []
    for entries in exports:
        require(isinstance(entries, list), "expected a JSON array exported from Cloud Logging")
        for entry in entries:
            require(isinstance(entry, dict), "invalid Cloud Logging record")
            request = entry.get("httpRequest")
            agent = request.get("userAgent") if isinstance(request, dict) else None
            payload = entry.get("jsonPayload")
            if isinstance(payload, dict) and "parentInsertId" in payload:
                if agent == GCS_ORIGIN_AGENT:
                    origin_entries.append(entry)
                continue
            if not matches(GCS_VIEWER_AGENT, agent):
                continue
            fingerprint = hashlib.sha256(agent.encode()).hexdigest()
            if fingerprint not in expected:
                continue
            require(isinstance(payload, dict), "missing Cloud CDN log payload")
            uri = request.get("requestUrl")
            require(isinstance(uri, str), "Cloud CDN log needs a request URL")
            url = urlsplit(uri)
            require(cloud_cdn_forwarding_rule(entry, project, rule)
                    and url.scheme == "https" and url.netloc in (base.netloc, f"{base.netloc}:443") and not url.query and not url.fragment
                    and request.get("requestMethod") == "GET" and url.path.startswith(prefix),
                    "Cloud CDN request was logged outside the approved forwarding rule, viewer host or repository prefix")
            status = request.get("status")
            size, fill = str(request.get("responseSize")), str(request.get("cacheFillBytes", 0))
            insert_id = entry.get("insertId")
            require(type(status) is int and 100 <= status <= 599 and re.fullmatch(r"[0-9]{1,19}", size)
                    and re.fullmatch(r"[0-9]{1,19}", fill) and isinstance(insert_id, str) and bool(insert_id),
                    "Cloud CDN log has invalid status, byte counts or log entry ID")
            key = url.path.removeprefix(base_path)
            record = {"status": status, "response_bytes": int(size), "cache_fill_bytes": int(fill),
                      "key_sha256": hashlib.sha256(key.encode()).hexdigest(),
                      "cache": cloud_cdn_cache_status(request, payload.get("statusDetails")), "origin_requests": {}}
            require(fingerprint not in records or (records[fingerprint] == record and inserts[fingerprint] == insert_id),
                    "Cloud CDN exports disagree about one viewer request")
            records[fingerprint], inserts[fingerprint] = record, insert_id
    viewers = {insert: viewer_id for viewer_id, insert in inserts.items()}
    require(len(viewers) == len(inserts), "Cloud CDN viewer log entry ID matched more than one request")
    for entry in origin_entries:
        parent = entry["jsonPayload"]["parentInsertId"]
        if not isinstance(parent, str) or parent not in viewers:
            continue
        require(cloud_cdn_forwarding_rule(entry, project, rule),
                "Cloud CDN origin request was logged outside the approved forwarding rule")
        request = entry["httpRequest"]
        insert_id, method, status = entry.get("insertId"), request.get("requestMethod"), request.get("status")
        fill = str(request.get("cacheFillBytes", 0))
        require(isinstance(insert_id, str) and bool(insert_id) and insert_id not in viewers
                and method in ("GET", "HEAD") and type(status) is int and 100 <= status <= 599
                and re.fullmatch(r"[0-9]{1,19}", fill),
                "Cloud CDN origin log has invalid method, status, byte count or log entry ID")
        fingerprint = hashlib.sha256(insert_id.encode()).hexdigest()
        requests = records[viewers[parent]]["origin_requests"]
        record = {"method": method, "status": status, "cache_fill_bytes": int(fill)}
        require(fingerprint not in requests or requests[fingerprint] == record,
                "Cloud CDN exports disagree about one origin request")
        requests[fingerprint] = record
    return records


def viewer_log_snapshot(fixture: dict, client: dict, digest: str, edge: str, args: argparse.Namespace) -> dict:
    proof = {"schema": f"crab.{edge}-viewer-requests/v1", "client_report_sha256": digest}
    if args.aws_log_stack is not None:
        with TemporaryDirectory(prefix="crab-cdn-aws-logs-") as temporary:
            viewer_logs, origin_logs = download_logs(fixture, client, args.aws_log_stack, args.aws_profile, Path(temporary))
            records = cloudfront_logs(viewer_logs, client, fixture)
            proof["origin_requests"] = s3_cloudfront_reads(origin_logs, records, fixture, client)
    elif edge == "cloudfront":
        records = cloudfront_logs(args.log, client, fixture)
        proof["origin_requests"] = s3_cloudfront_reads(args.s3_origin_log, records, fixture, client)
    elif args.azure_monitor:
        records, proof["origin_requests"] = azure_monitor_records(fixture, client, args.azure_origin_authority)
    elif edge == "front-door":
        records = front_door_logs((read_json_log(path) for path in args.log), client, fixture)
        origins = (read_json_log(path) for path in args.azure_blob_log)
        authority = read_json_log(args.azure_origin_authority)
        proof["origin_requests"] = azure_blob_front_door_reads(origins, authority, records, fixture, client)
    else:
        exports = gcp_logging_pages(fixture, client) if args.gcp_logging else (read_json_log(path) for path in args.log)
        records = cloud_cdn_logs(exports, client, fixture)
    return proof | {"records": records}


def viewer_evidence(fixture: dict, client: dict, digest: str, edge: str, args: argparse.Namespace) -> dict:
    deadline = time.monotonic() + args.log_wait_seconds
    # Viewer and origin records can arrive separately. Every attempt checks
    # a fresh snapshot, and invalid data or API errors stop collection.
    while True:
        proof = viewer_log_snapshot(fixture, client, digest, edge, args)
        records = proof["records"]
        if records.keys() == client["edge_responses"].keys():
            observed = verify_viewer_requests(proof, client, digest, edge)
            if observed.successful_fill or not args.require_origin_fill:
                return proof
            missing = "no logged fill from the approved CDN origin"
        else:
            missing = "CDN logs must cover each measured viewer attempt exactly once"
        remaining = deadline - time.monotonic()
        require(remaining > 0, missing)
        time.sleep(min(60, remaining))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--client", required=True, type=Path, help="exact timed client.json")
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--log", action="append", type=Path, default=[],
                        help="CloudFront W3C access log, Cloud Logging JSON export or Azure Monitor Front Door archive")
    source.add_argument("--aws-log-stack", help="download CloudFront and S3 logs from the approved companion stack")
    source.add_argument("--gcp-logging", action="store_true", help="read Cloud CDN logs for the approved fixture from Cloud Logging")
    source.add_argument("--azure-monitor", action="store_true", help="read Front Door and Blob archives through Azure Monitor settings")
    parser.add_argument("--s3-origin-log", action="append", type=Path, default=[],
                        help="plain-text S3 server access log from the approved CloudFront origin")
    parser.add_argument("--azure-blob-log", action="append", type=Path, default=[],
                        help="Azure Monitor StorageRead archive from the approved Blob origin")
    parser.add_argument("--azure-origin-authority", type=Path,
                        help="ARM Front Door profile JSON for private rows, Azure Public service tags JSON for public rows")
    parser.add_argument("--aws-profile", help="AWS CLI profile used for read-only companion log collection")
    parser.add_argument("--log-wait-seconds", type=int, default=0,
                        help="poll for missing records from the live log source within this window")
    parser.add_argument("--require-origin-fill", action="store_true",
                        help="require logged origin body bytes for a cold edge read")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        require(0 <= args.log_wait_seconds <= 24 * 60 * 60, "log wait must be between 0 and 86400 seconds")
        require(not args.log_wait_seconds or not args.log, "log polling requires a live provider source")
        require(not args.output.exists() and not args.output.is_symlink(), "viewer evidence output must be new")
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        edge, access, _, family, _ = scope_parts(fixture["scope"])
        require(PROVIDER_FOR_EDGE[edge] in VIEWER_REQUEST_FILES, "no viewer log collector is defined for this CDN fixture")
        require(not args.gcp_logging or edge == "cloud-cdn", "Cloud Logging requires a Cloud CDN fixture")
        require(not args.azure_monitor or edge == "front-door", "Azure Monitor collection requires a Front Door fixture")
        if args.aws_log_stack is not None:
            require(edge == "cloudfront" and not args.s3_origin_log,
                    "AWS log stack collection requires a CloudFront fixture and no local viewer or origin files")
        else:
            require(args.aws_profile is None, "AWS profile requires a companion stack")
            require(bool(args.s3_origin_log) == (edge == "cloudfront"),
                    "CloudFront viewer evidence requires S3 origin access logs")
        require(bool(args.azure_blob_log) == (edge == "front-door" and not args.azure_monitor),
                "Front Door file collection requires Blob origin logs")
        require(bool(args.azure_origin_authority) == (edge == "front-door" and (not args.azure_monitor or access == "public")),
                "Front Door file or public collection requires Azure origin authority")
        raw = read_git_record(args.client, limit=GIT_FETCH_RECORD_LIMIT)
        candidate = decode(raw)
        require(isinstance(candidate, dict) and CDN_PROVENANCE_FIELDS | {"arm", "platform"} <= candidate.keys()
                and candidate["arm"] in ("edge", "warm_local") and candidate["scope"] == fixture["scope"]
                and candidate["configuration_sha256"] == args.configuration_sha256,
                "CDN viewer evidence needs a fetch for the approved fixture")
        provenance = {key: candidate[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}}
        if is_direct_desktop_git_scope(fixture["scope"]):
            client = validate_desktop_git_read(candidate, fixture["scope"], "edge")
            export = client["reader"]["public_export_sha256"]
            require(client["reader"] == read_target_fingerprints(fixture, edge=True, export_sha256=export)
                    and (access == "public" or client["approval_sha256"] == issuer_settings(fixture)[2]),
                    "desktop viewer evidence requires the approved native target and CDN binding")
        elif is_direct_sdk_git_scope(fixture["scope"]):
            action = next((action for action in SDK_WORKTREE_VIEWER_FILES
                           if candidate.get("schema") == f"crab.cdn-sdk-git-{action}/v1"), None)
            client = validate_sdk_git_record(candidate, access, "edge", worktree_action=action)
            verify_git_traffic(client, edge, "CDN SDK Git action")
        elif is_direct_xorb_read_scope(fixture["scope"]):
            mount = is_direct_mount_xorb_scope(fixture["scope"])
            sparse_schema = MOUNT_XORB_RANGE_READ if mount else SDK_XORB_RANGE_READ
            sparse = candidate.get("schema") == sparse_schema
            expected = MOUNT_XORB_READ_FIELDS if mount else SDK_HYDRATION_READ_FIELDS
            if sparse:
                expected = expected | {"range_offset"}
                if not mount:
                    expected.add("commit")
            client = fields(candidate, expected, "CDN benchmark xorb read")
            key = fixture_object_key(fixture, "private", "xorb")
            schema = "crab.cdn-mount-xorb-read/v1" if mount else "crab.cdn-sdk-hydration/v1"
            if sparse:
                schema = sparse_schema
                require(mount or matches(GIT_OID, client["commit"]), "SDK range evidence requires a verified Git revision")
            require(client["schema"] == schema and client["arm"] == "edge"
                    and client["xorb_key_sha256"] == hashlib.sha256(key.encode()).hexdigest(),
                    "viewer evidence requires a timed read of the approved xorb")
            if mount:
                require(matches(GIT_OID, client["mounted_oid"]) and client["unmounted"] is True,
                        "mount viewer evidence requires a verified Git revision and clean detach")
            verify_git_traffic(client, edge, "CDN benchmark xorb read")
            if sparse:
                verify_sparse_xorb_traffic(client, fixture["size"])
        elif is_direct_service_scope(fixture["scope"]):
            client = validate_service_read(candidate, provenance, "edge")
            if family == "lfs":
                key = fixture_object_key(fixture, access, family)
                require(client["lfs_key_sha256"] == hashlib.sha256(key.encode()).hexdigest()
                        and client["payload_sha256"] == fixture["sha256"] and client["payload_bytes"] == fixture["size"],
                        "service viewer evidence requires the approved LFS object")
            reader = client["reader"]
            export = reader["public_export_sha256"]
            require(reader == read_target_fingerprints(fixture, edge=True, export_sha256=export),
                    "service viewer evidence requires the approved native target and CDN binding")
        else:
            client = validate_git_fetch(raw, provenance, candidate["arm"], "CDN benchmark fetch")
            require(client["observed_bytes"] == cli_payload_bytes(fixture),
                    "viewer evidence differs from the approved CLI payload size")
        require(client["all_edge_attempts"] > 0 and len(client["edge_responses"]) == client["all_edge_attempts"],
                "the timed fetch lacks a CDN request fingerprint for each viewer attempt")
        require(not args.require_origin_fill or client["arm"] == "edge", "origin fill requires a timed edge read")
        digest = hashlib.sha256(raw).hexdigest()
        proof = viewer_evidence(fixture, client, digest, edge, args)
        with args.output.open("x", encoding="utf-8") as destination:
            destination.write(json.dumps(proof, sort_keys=True) + "\n")
    except (ValueError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
