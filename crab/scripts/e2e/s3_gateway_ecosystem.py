#!/usr/bin/env python3
"""Qualify common object-store clients against a Crab S3 gateway."""

from __future__ import annotations

import argparse
import hashlib
import importlib.metadata
import io
import json
import os
import secrets
import shutil
import subprocess
import sys
import time
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Callable


ROOT = Path(__file__).resolve().parents[3]
CLIENT_ROOT = Path(__file__).with_name("s3_gateway_clients")


class QualificationError(RuntimeError):
    """A client did not preserve the qualification fixture."""


@dataclass(frozen=True)
class Context:
    endpoint: str
    bucket: str
    access_key: str
    secret_key: str
    session_token: str | None
    region: str
    prefix: str
    work_dir: Path
    duckdb: str
    aws: str
    s3cmd: str
    maven: str
    go: str

    def key(self, client: str, name: str) -> str:
        return f"{self.prefix}/{client}/{name}"

    def uri(self, client: str, name: str = "") -> str:
        key = self.key(client, name).rstrip("/")
        return f"s3://{self.bucket}/{key}"

    def process_env(self) -> dict[str, str]:
        environment = os.environ.copy()
        environment.update(
            {
                "AWS_ACCESS_KEY_ID": self.access_key,
                "AWS_SECRET_ACCESS_KEY": self.secret_key,
                "AWS_DEFAULT_REGION": self.region,
                "AWS_REGION": self.region,
                "AWS_ENDPOINT_URL_S3": self.endpoint,
                "AWS_EC2_METADATA_DISABLED": "true",
                "S3_GATEWAY_ECOSYSTEM_ENDPOINT": self.endpoint,
                "S3_GATEWAY_ECOSYSTEM_BUCKET": self.bucket,
                "S3_GATEWAY_ECOSYSTEM_PREFIX": self.prefix,
                "S3_GATEWAY_ECOSYSTEM_REGION": self.region,
                "S3_GATEWAY_ECOSYSTEM_ACCESS_KEY": self.access_key,
                "S3_GATEWAY_ECOSYSTEM_SECRET_KEY": self.secret_key,
                "NO_PROXY": "127.0.0.1,localhost",
                "no_proxy": "127.0.0.1,localhost",
            }
        )
        if self.session_token:
            environment["AWS_SESSION_TOKEN"] = self.session_token
            environment["S3_GATEWAY_ECOSYSTEM_SESSION_TOKEN"] = self.session_token
        return environment


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise QualificationError(message)


def _version(distribution: str) -> str:
    return importlib.metadata.version(distribution)


def _payload(label: str, size: int = 1024 * 1024 + 37) -> bytes:
    block = hashlib.sha256(label.encode("utf-8")).digest()
    return (block * ((size + len(block) - 1) // len(block)))[:size]


def _boto3_client(context: Context):
    import boto3
    from botocore.config import Config

    return boto3.client(
        "s3",
        endpoint_url=context.endpoint,
        region_name=context.region,
        aws_access_key_id=context.access_key,
        aws_secret_access_key=context.secret_key,
        aws_session_token=context.session_token,
        config=Config(
            retries={"mode": "standard", "max_attempts": 8},
            s3={"addressing_style": "path"},
        ),
    )


def _s3fs_options(context: Context) -> dict[str, object]:
    return {
        "key": context.access_key,
        "secret": context.secret_key,
        "token": context.session_token,
        "client_kwargs": {
            "endpoint_url": context.endpoint,
            "region_name": context.region,
        },
        "config_kwargs": {
            "retries": {"mode": "standard", "max_attempts": 8},
            "s3": {"addressing_style": "path"},
        },
        "use_ssl": urllib.parse.urlsplit(context.endpoint).scheme == "https",
    }


def _object_store_options(context: Context) -> dict[str, str]:
    return {
        "aws_access_key_id": context.access_key,
        "aws_secret_access_key": context.secret_key,
        "aws_region": context.region,
        "aws_endpoint": context.endpoint,
        "aws_allow_http": str(context.endpoint.startswith("http://")).lower(),
        "aws_virtual_hosted_style_request": "false",
        "aws_conditional_put": "etag",
    }


def _run(
    command: list[str],
    *,
    context: Context,
    cwd: Path | None = None,
    input_text: str | None = None,
    timeout: float = 600,
    environment: dict[str, str] | None = None,
) -> str:
    result = subprocess.run(
        command,
        cwd=cwd or ROOT,
        env=environment or context.process_env(),
        input=input_text,
        text=True,
        capture_output=True,
        timeout=timeout,
        check=False,
    )
    if result.returncode != 0:
        raise QualificationError(
            f"{Path(command[0]).name} exited with status {result.returncode}"
        )
    return result.stdout


def _test_aws_cli(context: Context) -> dict[str, object]:
    client_dir = context.work_dir / "aws-cli"
    client_dir.mkdir(parents=True, exist_ok=True)
    source = client_dir / "source.bin"
    destination = client_dir / "destination.bin"
    payload = _payload("aws-cli")
    source.write_bytes(payload)
    key = context.key("aws-cli", "object.bin")
    base = [context.aws, "--endpoint-url", context.endpoint, "s3api"]
    _run(
        base
        + [
            "put-object",
            "--bucket",
            context.bucket,
            "--key",
            key,
            "--body",
            str(source),
        ],
        context=context,
    )
    _run(
        base
        + [
            "get-object",
            "--bucket",
            context.bucket,
            "--key",
            key,
            "--range",
            "bytes=1048543-1048614",
            str(destination),
        ],
        context=context,
    )
    _require(destination.read_bytes() == payload[1048543:1048615], "AWS CLI range differs")
    url = _run(
        [
            context.aws,
            "--endpoint-url",
            context.endpoint,
            "s3",
            "presign",
            f"s3://{context.bucket}/{key}",
            "--expires-in",
            "300",
        ],
        context=context,
    ).strip()
    with urllib.request.urlopen(url, timeout=30) as response:
        presigned = response.read()
    _require(presigned == payload, "AWS CLI presigned GET differs")
    version = _run([context.aws, "--version"], context=context).strip()
    return {
        "version": version.split()[0],
        "bytes": len(payload),
        "range_bytes": destination.stat().st_size,
        "checks": ["put", "range_get", "presigned_get"],
    }


def _test_boto3(context: Context) -> dict[str, object]:
    client = _boto3_client(context)
    payload = _payload("boto3")
    key = context.key("boto3", "object.bin")
    client.put_object(Bucket=context.bucket, Key=key, Body=payload)
    head = client.head_object(Bucket=context.bucket, Key=key)
    _require(head["ContentLength"] == len(payload), "Boto3 HEAD length differs")
    body = client.get_object(
        Bucket=context.bucket,
        Key=key,
        Range="bytes=127-4222",
    )["Body"].read()
    _require(body == payload[127:4223], "Boto3 range differs")
    listing = client.list_objects_v2(
        Bucket=context.bucket,
        Prefix=context.key("boto3", ""),
    )
    listed_keys = [item["Key"] for item in listing.get("Contents", [])]
    _require(key in listed_keys, "Boto3 listing omitted the object")
    _require(len(listed_keys) == len(set(listed_keys)), "Boto3 listing duplicated a key")
    return {
        "version": _version("boto3"),
        "bytes": len(payload),
        "range_bytes": len(body),
        "checks": ["put", "head", "range_get", "list"],
    }


def _test_sigv4_presigned(context: Context) -> dict[str, object]:
    client = _boto3_client(context)
    payload = _payload("sigv4-presigned", 64 * 1024 + 11)
    key = context.key("sigv4-presigned", "object.bin")
    client.put_object(Bucket=context.bucket, Key=key, Body=payload)
    url = client.generate_presigned_url(
        "get_object",
        Params={"Bucket": context.bucket, "Key": key},
        ExpiresIn=300,
    )
    with urllib.request.urlopen(url, timeout=30) as response:
        actual = response.read()
    _require(actual == payload, "Boto3 presigned GET differs")
    return {
        "version": _version("botocore"),
        "bytes": len(payload),
        "checks": ["header_signed_put", "query_signed_get"],
    }


def _duckdb_secret(context: Context) -> str:
    parsed = urllib.parse.urlsplit(context.endpoint)

    def quote(value: str) -> str:
        return value.replace("'", "''")

    fields = [
        "TYPE s3",
        f"KEY_ID '{quote(context.access_key)}'",
        f"SECRET '{quote(context.secret_key)}'",
        f"REGION '{quote(context.region)}'",
        f"ENDPOINT '{quote(parsed.netloc)}'",
        "URL_STYLE 'path'",
        f"USE_SSL {'true' if parsed.scheme == 'https' else 'false'}",
    ]
    if context.session_token:
        fields.append(f"SESSION_TOKEN '{quote(context.session_token)}'")
    return "CREATE OR REPLACE SECRET crab_gateway (" + ", ".join(fields) + ");"


def _duckdb_fixture(context: Context, client_name: str) -> tuple[str, str]:
    import pyarrow as pa
    import pyarrow.parquet as pq

    local = context.work_dir / f"{client_name}-source.parquet"
    table = pa.table({"id": range(1000), "value": [index * 3 for index in range(1000)]})
    pq.write_table(table, local)
    input_key = context.key(client_name, "input.parquet")
    _boto3_client(context).upload_file(str(local), context.bucket, input_key)
    return f"s3://{context.bucket}/{input_key}", context.uri(client_name, "output.parquet")


def _test_duckdb_cli(context: Context) -> dict[str, object]:
    input_uri, output_uri = _duckdb_fixture(context, "duckdb-cli")
    sql = f"""
.mode list
.separator |
INSTALL httpfs;
LOAD httpfs;
{_duckdb_secret(context)}
COPY (SELECT * FROM read_parquet('{input_uri}') WHERE id % 2 = 0)
  TO '{output_uri}' (FORMAT PARQUET);
SELECT count(*), sum(value) FROM read_parquet('{output_uri}');
"""
    output = _run([context.duckdb], context=context, input_text=sql)
    _require("500|748500" in output, "DuckDB CLI aggregate differs")
    version = _run([context.duckdb, "--version"], context=context).strip()
    return {
        "version": version.split()[0].lstrip("v"),
        "rows": 500,
        "checks": ["parquet_read", "filtered_write", "parquet_readback"],
    }


def _test_duckdb_python(context: Context) -> dict[str, object]:
    import duckdb

    input_uri, output_uri = _duckdb_fixture(context, "duckdb-python")
    connection = duckdb.connect()
    try:
        connection.execute("INSTALL httpfs")
        connection.execute("LOAD httpfs")
        connection.execute(_duckdb_secret(context))
        connection.execute(
            f"COPY (SELECT * FROM read_parquet('{input_uri}') WHERE id % 2 = 1) "
            f"TO '{output_uri}' (FORMAT PARQUET)"
        )
        result = connection.execute(
            f"SELECT count(*), sum(value) FROM read_parquet('{output_uri}')"
        ).fetchone()
    finally:
        connection.close()
    _require(result == (500, 750000), "DuckDB Python aggregate differs")
    return {
        "version": duckdb.__version__,
        "rows": result[0],
        "checks": ["parquet_read", "filtered_write", "parquet_readback"],
    }


def _test_lancedb(context: Context) -> dict[str, object]:
    import lancedb

    database = lancedb.connect(
        context.uri("lancedb", "database"),
        storage_options=_object_store_options(context),
    )
    table = database.create_table(
        "vectors",
        data=[
            {"id": "zero", "vector": [0.0, 0.0, 0.0]},
            {"id": "one", "vector": [1.0, 1.0, 1.0]},
        ],
        mode="overwrite",
    )
    table.add([{"id": "near", "vector": [0.1, 0.1, 0.1]}])
    result = table.search([0.05, 0.05, 0.05]).limit(2).to_arrow()
    ids = set(result.column("id").to_pylist())
    _require(ids == {"zero", "near"}, "LanceDB vector search differs")
    reopened = database.open_table("vectors")
    _require(reopened.count_rows() == 3, "LanceDB row count differs")
    return {
        "version": lancedb.__version__,
        "rows": 3,
        "checks": ["create", "append", "vector_search", "reopen"],
    }


def _test_pyarrow(context: Context) -> dict[str, object]:
    import pyarrow as pa
    import pyarrow.fs as fs
    import pyarrow.parquet as pq

    parsed = urllib.parse.urlsplit(context.endpoint)
    filesystem = fs.S3FileSystem(
        access_key=context.access_key,
        secret_key=context.secret_key,
        session_token=context.session_token,
        region=context.region,
        scheme=parsed.scheme,
        endpoint_override=parsed.netloc,
        force_virtual_addressing=False,
        background_writes=False,
    )
    table = pa.table({"id": range(2000), "value": [index * 7 for index in range(2000)]})
    path = f"{context.bucket}/{context.key('pyarrow', 'data.parquet')}"
    pq.write_table(table, path, filesystem=filesystem)
    actual = pq.read_table(path, filesystem=filesystem, filters=[("id", ">=", 1900)])
    _require(actual.num_rows == 100, "PyArrow filtered row count differs")
    _require(sum(actual.column("value").to_pylist()) == 1364650, "PyArrow values differ")
    return {
        "version": pa.__version__,
        "rows": table.num_rows,
        "checks": ["parquet_write", "ranged_filtered_read"],
    }


def _test_pandas(context: Context) -> dict[str, object]:
    import pandas as pd

    frame = pd.DataFrame({"id": range(1200), "value": [index * 5 for index in range(1200)]})
    uri = context.uri("pandas", "data.parquet")
    options = _s3fs_options(context)
    frame.to_parquet(uri, index=False, storage_options=options)
    actual = pd.read_parquet(uri, storage_options=options)
    _require(actual.equals(frame), "Pandas Parquet round trip differs")
    return {
        "version": pd.__version__,
        "rows": len(actual),
        "checks": ["parquet_write", "parquet_read"],
    }


def _test_polars(context: Context) -> dict[str, object]:
    import polars as pl

    frame = pl.DataFrame({"id": range(1400), "value": [index * 11 for index in range(1400)]})
    uri = context.uri("polars", "data.parquet")
    options = _object_store_options(context)
    frame.write_parquet(uri, storage_options=options)
    actual = pl.read_parquet(uri, storage_options=options)
    _require(actual.equals(frame), "Polars Parquet round trip differs")
    return {
        "version": pl.__version__,
        "rows": actual.height,
        "checks": ["parquet_write", "parquet_read"],
    }


def _test_fsspec_s3fs(context: Context) -> dict[str, object]:
    import fsspec
    import s3fs

    options = _s3fs_options(context)
    filesystem = fsspec.filesystem("s3", **options)
    payload = _payload("fsspec-s3fs", 256 * 1024 + 19)
    path = f"{context.bucket}/{context.key('fsspec-s3fs', 'object.bin')}"
    with filesystem.open(path, "wb") as stream:
        stream.write(payload)
    with filesystem.open(path, "rb") as stream:
        stream.seek(4093)
        actual = stream.read(8192)
    _require(actual == payload[4093:12285], "fsspec/s3fs range differs")
    _require(filesystem.size(path) == len(payload), "fsspec/s3fs size differs")
    return {
        "version": f"fsspec={fsspec.__version__},s3fs={s3fs.__version__}",
        "bytes": len(payload),
        "range_bytes": len(actual),
        "checks": ["write", "seek_read", "stat"],
    }


def _test_dask(context: Context) -> dict[str, object]:
    import dask
    import dask.dataframe as dd
    import pandas as pd

    frame = pd.DataFrame({"id": range(1600), "value": [index * 13 for index in range(1600)]})
    dataset = dd.from_pandas(frame, npartitions=4)
    uri = context.uri("dask", "dataset")
    options = _s3fs_options(context)
    dataset.to_parquet(uri, storage_options=options, write_index=False)
    actual = dd.read_parquet(uri, storage_options=options).compute().sort_values("id")
    actual.reset_index(drop=True, inplace=True)
    _require(actual.equals(frame), "Dask Parquet round trip differs")
    return {
        "version": dask.__version__,
        "rows": len(actual),
        "partitions": 4,
        "checks": ["partitioned_write", "parallel_read"],
    }


def _test_minio(context: Context) -> dict[str, object]:
    from minio import Minio

    parsed = urllib.parse.urlsplit(context.endpoint)
    client = Minio(
        parsed.netloc,
        access_key=context.access_key,
        secret_key=context.secret_key,
        session_token=context.session_token,
        secure=parsed.scheme == "https",
        region=context.region,
    )
    payload = _payload("minio", 384 * 1024 + 23)
    key = context.key("minio", "object.bin")
    client.put_object(context.bucket, key, io.BytesIO(payload), len(payload))
    response = client.get_object(context.bucket, key, offset=8191, length=16384)
    try:
        actual = response.read()
    finally:
        response.close()
        response.release_conn()
    _require(actual == payload[8191:24575], "MinIO range differs")
    _require(client.stat_object(context.bucket, key).size == len(payload), "MinIO stat differs")
    return {
        "version": _version("minio"),
        "bytes": len(payload),
        "range_bytes": len(actual),
        "checks": ["put", "range_get", "stat"],
    }


def _test_smart_open(context: Context) -> dict[str, object]:
    import smart_open

    client = _boto3_client(context)
    payload = _payload("smart-open", 192 * 1024 + 29)
    uri = context.uri("smart-open", "object.bin")
    transport = {"client": client}
    with smart_open.open(uri, "wb", transport_params=transport) as stream:
        stream.write(payload)
    with smart_open.open(uri, "rb", transport_params=transport) as stream:
        actual = stream.read()
    _require(actual == payload, "smart_open round trip differs")
    return {
        "version": smart_open.__version__,
        "bytes": len(payload),
        "checks": ["write", "read"],
    }


def _test_awswrangler(context: Context) -> dict[str, object]:
    import awswrangler as wr
    import boto3
    import pandas as pd

    session = boto3.Session(
        aws_access_key_id=context.access_key,
        aws_secret_access_key=context.secret_key,
        aws_session_token=context.session_token,
        region_name=context.region,
    )
    frame = pd.DataFrame({"id": range(1800), "value": [index * 17 for index in range(1800)]})
    uri = context.uri("awswrangler", "data.parquet")
    previous_endpoint = wr.config.s3_endpoint_url
    wr.config.s3_endpoint_url = context.endpoint
    try:
        wr.s3.to_parquet(frame, path=uri, dataset=False, index=False, boto3_session=session)
        actual = wr.s3.read_parquet(path=uri, boto3_session=session)
    finally:
        wr.config.s3_endpoint_url = previous_endpoint
    actual = actual.astype({"id": "int64", "value": "int64"})
    _require(actual.equals(frame), "awswrangler Parquet round trip differs")
    return {
        "version": wr.__version__,
        "rows": len(actual),
        "checks": ["parquet_write", "parquet_read"],
    }


def _test_pyiceberg(context: Context) -> dict[str, object]:
    import pyarrow as pa
    import pyiceberg
    from pyiceberg.catalog import load_catalog
    from pyiceberg.schema import Schema
    from pyiceberg.types import LongType, NestedField, StringType

    database = context.work_dir / "pyiceberg-catalog.db"
    catalog = load_catalog(
        "qualification",
        type="sql",
        uri=f"sqlite:///{database}",
        warehouse=context.uri("pyiceberg", "warehouse"),
        **{
            "py-io-impl": "pyiceberg.io.pyarrow.PyArrowFileIO",
            "s3.endpoint": context.endpoint,
            "s3.access-key-id": context.access_key,
            "s3.secret-access-key": context.secret_key,
            "s3.session-token": context.session_token,
            "s3.region": context.region,
            "s3.force-virtual-addressing": "false",
        },
    )
    catalog.create_namespace("battle")
    schema = Schema(
        NestedField(1, "id", LongType(), required=False),
        NestedField(2, "value", StringType(), required=False),
    )
    table = catalog.create_table(("battle", "events"), schema=schema)
    first = pa.table({"id": range(600), "value": [f"first-{index}" for index in range(600)]})
    second = pa.table({"id": range(600, 1000), "value": [f"second-{index}" for index in range(600, 1000)]})
    table.append(first)
    table.append(second)
    actual = catalog.load_table(("battle", "events")).scan().to_arrow()
    _require(actual.num_rows == 1000, "PyIceberg row count differs")
    _require(len(table.metadata.snapshots) == 2, "PyIceberg snapshot count differs")
    return {
        "version": pyiceberg.__version__,
        "rows": actual.num_rows,
        "snapshots": len(table.metadata.snapshots),
        "checks": ["create", "append", "second_snapshot", "scan"],
    }


def _test_delta_lake(context: Context) -> dict[str, object]:
    import pyarrow as pa
    from deltalake import DeltaTable, __version__, write_deltalake

    uri = context.uri("delta-lake", "table")
    options = {
        "AWS_ACCESS_KEY_ID": context.access_key,
        "AWS_SECRET_ACCESS_KEY": context.secret_key,
        "AWS_REGION": context.region,
        "AWS_ENDPOINT_URL": context.endpoint,
        "AWS_ALLOW_HTTP": str(context.endpoint.startswith("http://")).lower(),
        "AWS_S3_ADDRESSING_STYLE": "path",
        "AWS_S3_ALLOW_UNSAFE_RENAME": "true",
        "AWS_CONDITIONAL_PUT": "etag",
    }
    if context.session_token:
        options["AWS_SESSION_TOKEN"] = context.session_token
    first = pa.table({"id": range(700), "value": [index * 19 for index in range(700)]})
    second = pa.table({"id": range(700, 1000), "value": [index * 19 for index in range(700, 1000)]})
    write_deltalake(uri, first, mode="overwrite", storage_options=options)
    write_deltalake(uri, second, mode="append", storage_options=options)
    table = DeltaTable(uri, storage_options=options)
    actual = table.to_pyarrow_table()
    _require(actual.num_rows == 1000, "Delta Lake row count differs")
    _require(table.version() == 1, "Delta Lake version differs")
    return {
        "version": __version__,
        "rows": actual.num_rows,
        "table_version": table.version(),
        "checks": ["overwrite", "append", "transaction_log", "read"],
    }


def _test_pyspark(context: Context) -> dict[str, object]:
    import pyspark
    from pyspark.sql import SparkSession

    parsed = urllib.parse.urlsplit(context.endpoint)
    ivy = context.work_dir / "spark-ivy"
    builder = (
        SparkSession.builder.master("local[2]")
        .appName("crab-s3-gateway-qualification")
        .config(
            "spark.jars.packages",
            "org.apache.hadoop:hadoop-aws:3.5.0,software.amazon.awssdk:bundle:2.35.4",
        )
        .config("spark.jars.ivy", str(ivy))
        .config("spark.hadoop.fs.s3a.impl", "org.apache.hadoop.fs.s3a.S3AFileSystem")
        .config("spark.hadoop.fs.s3a.endpoint", context.endpoint)
        .config("spark.hadoop.fs.s3a.endpoint.region", context.region)
        .config("spark.hadoop.fs.s3a.path.style.access", "true")
        .config("spark.hadoop.fs.s3a.connection.ssl.enabled", str(parsed.scheme == "https").lower())
        .config(
            "spark.hadoop.fs.s3a.aws.credentials.provider",
            "org.apache.hadoop.fs.s3a.SimpleAWSCredentialsProvider",
        )
        .config("spark.hadoop.fs.s3a.access.key", context.access_key)
        .config("spark.hadoop.fs.s3a.secret.key", context.secret_key)
        .config("spark.hadoop.fs.s3a.change.detection.mode", "none")
        .config("spark.hadoop.mapreduce.fileoutputcommitter.algorithm.version", "2")
        .config("spark.sql.shuffle.partitions", "2")
    )
    if context.session_token:
        builder = builder.config("spark.hadoop.fs.s3a.session.token", context.session_token)
    spark = builder.getOrCreate()
    spark.sparkContext.setLogLevel("ERROR")
    uri = f"s3a://{context.bucket}/{context.key('pyspark', 'dataset')}"
    try:
        spark.range(0, 2000).selectExpr("id", "id * 23 AS value").repartition(2).write.mode(
            "overwrite"
        ).parquet(uri)
        row = spark.read.parquet(uri).selectExpr("count(*) AS count", "sum(value) AS total").first()
    finally:
        spark.stop()
    _require(row["count"] == 2000, "PySpark row count differs")
    _require(row["total"] == 45977000, "PySpark values differ")
    return {
        "version": pyspark.__version__,
        "rows": row["count"],
        "partitions": 2,
        "checks": ["s3a_write", "rename_commit", "s3a_read"],
    }


def _test_s3cmd(context: Context) -> dict[str, object]:
    client_dir = context.work_dir / "s3cmd"
    client_dir.mkdir(parents=True, exist_ok=True)
    config = client_dir / "config"
    source = client_dir / "source.bin"
    destination = client_dir / "destination.bin"
    parsed = urllib.parse.urlsplit(context.endpoint)
    config.write_text(
        "\n".join(
            [
                "[default]",
                f"access_key = {context.access_key}",
                f"secret_key = {context.secret_key}",
                f"access_token = {context.session_token or ''}",
                f"host_base = {parsed.netloc}",
                f"host_bucket = {parsed.netloc}/%(bucket)",
                f"use_https = {'True' if parsed.scheme == 'https' else 'False'}",
                "signature_v2 = False",
            ]
        )
        + "\n"
    )
    config.chmod(0o600)
    payload = _payload("s3cmd", 128 * 1024 + 31)
    source.write_bytes(payload)
    uri = context.uri("s3cmd", "object.bin")
    command = [context.s3cmd, "--config", str(config), "--no-progress"]
    try:
        _run(command + ["put", str(source), uri], context=context)
        _run(command + ["get", "--force", uri, str(destination)], context=context)
        prefix_uri = f"s3://{context.bucket}/{context.key('s3cmd', '')}"
        listing = _run(command + ["ls", prefix_uri], context=context)
    finally:
        config.unlink(missing_ok=True)
    _require(destination.read_bytes() == payload, "s3cmd round trip differs")
    _require("object.bin" in listing, "s3cmd listing differs")
    return {
        "version": _version("s3cmd"),
        "bytes": len(payload),
        "checks": ["put", "get", "list"],
    }


def _test_java_aws_sdk_v2(context: Context) -> dict[str, object]:
    source = CLIENT_ROOT / "java"
    build = context.work_dir / "java-target"
    maven_repo = context.work_dir / "maven-repository"
    output = _run(
        [
            context.maven,
            "-q",
            "-f",
            str(source / "pom.xml"),
            f"-Dmaven.repo.local={maven_repo}",
            f"-Dqualification.build.directory={build}",
            "compile",
            "exec:java",
            "-Dexec.mainClass=org.crabbuild.qualification.S3GatewayQualification",
        ],
        context=context,
        timeout=900,
    )
    _require("java-aws-sdk-v2:passed" in output, "Java AWS SDK result missing")
    return {
        "version": "2.35.4",
        "checks": ["put", "range_get", "list"],
    }


def _test_go_aws_sdk_v2(context: Context) -> dict[str, object]:
    source = CLIENT_ROOT / "go"
    environment = context.process_env()
    environment["GOCACHE"] = str(context.work_dir / "go-build-cache")
    environment["GOMODCACHE"] = str(context.work_dir / "go-module-cache")
    output = _run(
        [context.go, "run", "."],
        context=context,
        cwd=source,
        timeout=900,
        environment=environment,
    )
    _require("go-aws-sdk-v2:passed" in output, "Go AWS SDK result missing")
    return {
        "version": "s3=1.113.1",
        "checks": ["put", "range_get", "list"],
    }


CLIENTS: dict[str, Callable[[Context], dict[str, object]]] = {
    "aws_cli": _test_aws_cli,
    "boto3": _test_boto3,
    "sigv4_presigned": _test_sigv4_presigned,
    "duckdb_cli": _test_duckdb_cli,
    "duckdb_python": _test_duckdb_python,
    # Launch subprocess-backed runtimes before LanceDB loads its native runtime,
    # which warns when a process forks after initialization.
    "pyspark_s3a": _test_pyspark,
    "java_aws_sdk_v2": _test_java_aws_sdk_v2,
    "go_aws_sdk_v2": _test_go_aws_sdk_v2,
    "s3cmd": _test_s3cmd,
    "lancedb": _test_lancedb,
    "pyarrow": _test_pyarrow,
    "pandas": _test_pandas,
    "polars": _test_polars,
    "fsspec_s3fs": _test_fsspec_s3fs,
    "dask": _test_dask,
    "minio": _test_minio,
    "smart_open": _test_smart_open,
    "awswrangler": _test_awswrangler,
    "pyiceberg": _test_pyiceberg,
    "delta_lake": _test_delta_lake,
}


def build_report(
    prefix: str,
    results: dict[str, dict[str, object]],
    elapsed_ms: int,
) -> dict[str, object]:
    expected = set(CLIENTS)
    coverage_complete = set(results) == expected
    passed = bool(results) and all(
        result.get("status") == "passed" for result in results.values()
    )
    return {
        "schema": "crab.s3-gateway-ecosystem",
        "schema_version": 1,
        "status": "passed" if passed else "failed",
        "run_prefix_sha256": hashlib.sha256(prefix.encode("utf-8")).hexdigest(),
        "elapsed_ms": elapsed_ms,
        "coverage": {
            "complete": coverage_complete,
            "expected_clients": sorted(expected),
            "measured_clients": sorted(results),
        },
        "clients": results,
    }


def _write_report(path: Path, report: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def parser() -> argparse.ArgumentParser:
    argument_parser = argparse.ArgumentParser()
    argument_parser.add_argument("--endpoint", required=True)
    argument_parser.add_argument("--bucket", required=True)
    argument_parser.add_argument("--work-dir", type=Path, required=True)
    argument_parser.add_argument("--report", type=Path, required=True)
    argument_parser.add_argument("--prefix", default="main/qualification/ecosystem")
    argument_parser.add_argument("--region", default="us-east-1")
    argument_parser.add_argument("--clients", default=",".join(CLIENTS))
    argument_parser.add_argument("--duckdb", default="duckdb")
    argument_parser.add_argument("--aws", default="aws")
    argument_parser.add_argument("--s3cmd", default="s3cmd")
    argument_parser.add_argument("--maven", default="mvn")
    argument_parser.add_argument("--go", default="go")
    return argument_parser


def _credential(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise QualificationError(f"{name} is required")
    return value


def main() -> int:
    argument_parser = parser()
    args = argument_parser.parse_args()
    parsed = urllib.parse.urlsplit(args.endpoint)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname:
        argument_parser.error("--endpoint must be an absolute HTTP or HTTPS URL")
    if parsed.path not in {"", "/"} or parsed.query or parsed.fragment:
        argument_parser.error("--endpoint must not contain a path, query, or fragment")
    if not args.work_dir.is_absolute() or not args.report.is_absolute():
        argument_parser.error("--work-dir and --report must be absolute paths")
    selected = [name.strip() for name in args.clients.split(",") if name.strip()]
    unknown = sorted(set(selected) - CLIENTS.keys())
    if not selected or unknown:
        argument_parser.error(f"--clients contains unknown values: {','.join(unknown)}")
    missing_commands = [
        command
        for name, command in {
            "aws_cli": args.aws,
            "duckdb_cli": args.duckdb,
            "go_aws_sdk_v2": args.go,
            "java_aws_sdk_v2": args.maven,
            "s3cmd": args.s3cmd,
        }.items()
        if name in selected and shutil.which(command) is None
    ]
    if missing_commands:
        argument_parser.error(f"required commands are missing: {','.join(missing_commands)}")
    try:
        args.work_dir.mkdir(parents=True, exist_ok=True)
        prefix = f"{args.prefix.strip('/')}/{secrets.token_hex(8)}"
        context = Context(
            endpoint=args.endpoint.rstrip("/"),
            bucket=args.bucket,
            access_key=_credential("S3_GATEWAY_ECOSYSTEM_ACCESS_KEY"),
            secret_key=_credential("S3_GATEWAY_ECOSYSTEM_SECRET_KEY"),
            session_token=os.environ.get("S3_GATEWAY_ECOSYSTEM_SESSION_TOKEN"),
            region=args.region,
            prefix=prefix,
            work_dir=args.work_dir,
            duckdb=args.duckdb,
            aws=args.aws,
            s3cmd=args.s3cmd,
            maven=args.maven,
            go=args.go,
        )
        results: dict[str, dict[str, object]] = {}
        suite_started = time.monotonic()
        for name in selected:
            started = time.monotonic()
            try:
                details = CLIENTS[name](context)
                details.update(
                    {
                        "status": "passed",
                        "elapsed_ms": round((time.monotonic() - started) * 1000),
                    }
                )
                results[name] = details
                print(f"{name}: passed", flush=True)
            except Exception as error:
                results[name] = {
                    "status": "failed",
                    "elapsed_ms": round((time.monotonic() - started) * 1000),
                    "error_type": type(error).__name__,
                }
                print(f"{name}: failed ({type(error).__name__})", file=sys.stderr, flush=True)
        report = build_report(
            prefix,
            results,
            round((time.monotonic() - suite_started) * 1000),
        )
        _write_report(args.report, report)
        print(json.dumps(report, sort_keys=True))
        return 0 if report["status"] == "passed" else 1
    except (OSError, QualificationError, ValueError) as error:
        print(f"error: ecosystem qualification failed ({type(error).__name__})", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
