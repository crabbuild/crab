"""Read Azure Monitor archives for the approved Front Door and Blob resources."""

from __future__ import annotations

import re
from collections.abc import Iterator
from datetime import datetime, timedelta, timezone
from email.utils import formatdate
from urllib.parse import quote, urlsplit

from cdn_qualification import decode, require
from collect_front_door_deployment_policy import RESOURCE_GROUP, SUBSCRIPTION, arm_document
from run_cdn_transport_checks import https_response


STORAGE_ACCOUNT = re.compile(
    rf"/subscriptions/{SUBSCRIPTION.pattern}/resourcegroups/{RESOURCE_GROUP.pattern}"
    r"/providers/microsoft\.storage/storageaccounts/([a-z0-9]{3,24})",
    re.IGNORECASE,
)
DIAGNOSTIC_SETTINGS = "2021-05-01-preview"
MAX_ARCHIVE_HOURS = 4


def archive_account(resource: str, category: str, authorization: str) -> str:
    response = arm_document(f"{resource}/providers/Microsoft.Insights/diagnosticSettings",
                            DIAGNOSTIC_SETTINGS, authorization)
    entries = response.get("value")
    require(isinstance(entries, list) and not response.get("nextLink"), "incomplete Azure diagnostic settings inventory")
    accounts = set()
    for setting in entries:
        props = setting.get("properties") if isinstance(setting, dict) else None
        require(isinstance(props, dict), "invalid Azure diagnostic setting")
        account = props.get("storageAccountId")
        if not account:
            continue
        logs = props.get("logs", [])
        require(isinstance(logs, list) and all(isinstance(log, dict) for log in logs),
                "invalid Azure diagnostic log settings")
        if any(log.get("enabled") is True and (log.get("category") == category or log.get("categoryGroup") == "allLogs")
               for log in logs):
            match = STORAGE_ACCOUNT.fullmatch(account) if isinstance(account, str) else None
            require(match is not None, "invalid Azure diagnostic storage account ID")
            accounts.add(match[1].lower())
    require(len(accounts) == 1, f"expected one Azure storage destination for {category}")
    return accounts.pop()


def azure_log_archives(resource: str, category: str, client: dict, arm: str, storage: str, limit: int) -> Iterator[object]:
    first = (client["started_at_unix_ms"] - 60_000) // 3_600_000
    last = (client["finished_at_unix_ms"] + 60_000) // 3_600_000
    require(last - first < MAX_ARCHIVE_HOURS, "Azure log measurement spans too many archive hours")
    account = archive_account(resource, category, arm)
    base = urlsplit(f"https://{account}.blob.core.windows.net")
    _, _, subscription, tail = resource.split("/", 3)
    # Azure Monitor uppercases the ARM path segments while the subscription
    # GUID in the documented archive path stays lowercase.
    prefix = f"/insights-logs-{category.lower()}/resourceId=/SUBSCRIPTIONS/{subscription}/{tail.upper()}"
    headers = {"Authorization": storage, "x-ms-version": "2023-11-03",
               "x-ms-date": formatdate(usegmt=True), "Accept-Encoding": "identity"}
    epoch = datetime(1970, 1, 1, tzinfo=timezone.utc)
    remaining = limit
    for hour in range(first, last + 1):
        try:
            at = epoch + timedelta(hours=hour)
        except OverflowError:
            raise ValueError("invalid Azure log measurement interval") from None
        name = quote(f"{prefix}/{at:y=%Y/m=%m/d=%d/h=%H}/m=00/PT1H.json", safe="/=")
        with https_response(base, "GET", name, headers, "Azure Monitor archive") as response:
            if response.status == 404 and response.getheader("x-ms-error-code") in ("BlobNotFound", "ContainerNotFound"):
                continue
            require(response.status == 200, f"Azure Monitor archive expected HTTP 200, got {response.status}")
            raw = response.read(remaining + 1)
        remaining -= len(raw)
        require(remaining >= 0, f"Azure {category} archives exceed the JSON size limit")
        try:
            archive = decode(raw)
        except (ValueError, RecursionError):
            raise ValueError(f"invalid Azure {category} archive") from None
        yield archive
