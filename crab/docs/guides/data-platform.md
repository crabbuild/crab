# ETL and Data Platform Guide

Use Crab to pin repository inputs, warm caches, emit replayable lineage, and
export catalog inventory. Table engines still own table transactions and query
planning.

## Pinned Inputs and Lineage

```python
manifest = crab.prepare_etl_lineage(
    "crab://lake/events",
    ["tables/events/**/*.parquet"],
    rev="main",
    job_name="daily-events",
    run_id="airflow-2026-07-06T00:00Z",
    output_path="lineage.json",
    prefetch_profile="daily-etl",
    output_refs=["refs/heads/candidates/daily-events"],
)
```

Replay with each input's `repo_url`, `resolved_rev`, and `path`.

## Catalog Export

```python
catalog = crab.export_catalog(
    "crab://lake/events",
    rev="daily-2026-07-06",
    prefix="tables/events/",
    labels={"dataset": "events", "table_format": "delta"},
    output_path="catalog-events.json",
)
```

Catalog exports include refs, bounded commit history, path inventory, sizes,
pointer kinds, hash namespaces, hashes, and dataset labels.

## Operations

- Run `auth_status()` before long scans.
- Put SDK caches on local SSD for workers.
- Use prefetch profiles to bound startup egress.
- Persist lineage, catalog exports, logs, and candidate refs together.
