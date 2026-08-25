# Crab Cost Optimizer Guide

The cost optimizer helps you understand and reduce your object-storage
bill. It collects an inventory of your bucket, applies a versioned
pricing model, and produces actionable recommendations.

## Quick Start

```bash
# Run the cost report (human-readable)
crab doctor --cost

# Build one operator workflow from cost, tiering, cache, replica, and xorb checks
crab optimize plan

# Apply safe configured steps; add --include-xorbs to run the remote xorb rewrite too
crab optimize apply

# JSON output for automation
crab doctor --cost --json
crab optimize plan --json

# Per-class storage breakdown
crab stat classes
crab stat classes --json
```

## Inventory Sources

The cost optimizer needs to know what's in your bucket. It supports
three inventory sources:

### Live walk (default for small buckets)

Uses `object_store::list` to stream all objects under the Crab
prefixes. Bounded by `cost.list_concurrency` (default 32, max 128).

```toml
[cost]
inventory_source = "live"
list_concurrency = 32
```

### Provider-side reports

Provider-side report parsers exist as library components, but Crab does not
yet have a report-location/discovery contract. An explicit `report` source
therefore fails with a configuration error; use `live` until report wiring is
available.

### Auto (default)

Selects live inventory today. The report freshness setting is reserved for
the provider-report discovery path.

```toml
[cost]
inventory_source = "auto"
report_max_staleness_hours = 48
```

## Sampling

For very large buckets, you can sample the inventory to trade accuracy
for speed:

```bash
crab doctor --cost --sample 0.25
```

This uses a deterministic `blake3(key)` hash to include ~25% of
objects. The report records that totals are scaled estimates when sampling
is enabled; it does not currently emit a statistical confidence interval.

The optional `--top-k` cold-object report is bounded to 10,000 entries so a
large archive inventory cannot turn the report into unbounded memory use.

## Pricing Model

### Embedded prices

Crab ships with a versioned price table generated at build time from
`pricing/data/<version>.yaml`. The version appears in every report as
`price_table_version`.

### Override file

You can override specific prices for your contract rates:

```yaml
# ~/.crab/pricing-override.yaml
version: "my-contract-2026"
providers:
  aws:
    regions:
      us-east-1:
        classes:
          Standard:
            gb_month_usd: "0.018"
          Standard-IA:
            gb_month_usd: "0.010"
```

```toml
[cost]
pricing_file = "~/.crab/pricing-override.yaml"
```

Override fields replace embedded values; missing fields inherit from
the embedded table. Unknown fields emit a warning but don't fail.

**Security:** On unix, the override file must have `0600` permissions
(owner read/write only). Looser permissions trigger a warning. On
Windows, an informational note is emitted.

### Monetary precision

All monetary math uses `rust_decimal::Decimal` throughout. Display
shows 2 decimal places; JSON output uses 6 decimal places. No
floating-point drift.

## Reading the Report

The cost report has five sections:

### 1. Header

Shows the price table version, override version (if any), generation
timestamp, and inventory source.

### 2. Cost Summary

- **Current monthly cost** — estimated from current class distribution.
- **Projected (with tier)** — equal to current cost until object age and
  access telemetry are available to model tier transitions.
- **Estimated savings** — conservative projected savings; use enabled
  recommendations for potential tier savings.

### 3. Per-Class Breakdown

A table showing each storage class with its size, share of total, and
monthly cost.

### 4. Recommendations

Actionable suggestions, each with:

- **Title** — what to do.
- **Rationale** — why it helps.
- **Action command** — the `crab` command to run.
- **Savings** — estimated monthly savings in USD.
- **Risk level** — low, medium, or high.
- **Dependencies** — other recommendations that should be applied first.

Recommendations are never auto-executed. They are informational only.

## Operator Workflow

`crab optimize plan` turns the lower-level maintenance surfaces into one
operator checklist:

- `crab doctor --cost` for cost inventory and recommendations.
- active-active maintenance admission before any mutating apply step.
- lifecycle tiering through `crab tier plan --apply --merge` when
  `[tier] enabled = true`; provider conditional-write and read-back checks
  remain in force.
- xorb optimization through the reconciled xorb, shard, and file-index
  transaction path. Use `crab optimize xorbs --dry-run` for estimates.
- local cache pruning through `crab prune`.
- replica policy checks through `crab replica doctor --fix-plan` when
  replication is configured.

`crab optimize apply` executes the same plan in order. It preserves each
underlying command's safety checks, holds a repository-wide apply lock, and
stops on the first failed step. Child output is drained with a fixed memory
bound, and invalid sampling or unavailable report-inventory requests fail
before apply. Xorb rewrites are opt-in because they can be long-running and
may restore archived objects before rewriting content-addressed xorbs.

### 5. Heaviest Cold Objects

The top-K largest objects in archive classes, useful for identifying
restore cost hotspots.

## Built-in Recommendations

| Rule | Fires when | Risk |
|------|-----------|------|
| `apply_tier_plan_ia` | Standard bytes could save >$1/mo in IA | Low |
| `apply_tier_plan_glacier` | IA bytes could save >$1/mo in Glacier | Medium |
| `enable_intelligent_tiering` | S3 Standard bytes with unpredictable access | Low |
| `optimize_xorbs_profile_mismatch` | Avg xorb size >1.5x from nearest profile target | Medium |
| `gc_candidates` | Orphan objects detected | Low |

## Configuration Reference

```toml
[cost]
# Inventory source: auto, live, or report
inventory_source = "auto"

# Maximum concurrent LIST requests for live walks
list_concurrency = 32

# Sample ratio (1.0 = no sampling)
sample_ratio = 1.0

# Path to pricing override YAML file
pricing_file = ""

# Access window in days for cost analysis
access_window_days = 90

# Reserved; true fails closed until billing-account scope is defined
apply_free_tier = false

# Maximum staleness in hours for inventory reports
report_max_staleness_hours = 48
```

Environment variable overrides follow the `CRAB_COST_*` convention:

- `CRAB_COST_INVENTORY_SOURCE`
- `CRAB_COST_LIST_CONCURRENCY`
- `CRAB_COST_SAMPLE_RATIO`
- `CRAB_COST_PRICING_FILE`

## JSON Schema

The JSON output conforms to `crab/schemas/cost.json` with schema
name `"cost"` version `"1.0"`.

The `stat classes` JSON output conforms to `crab/schemas/stat.classes.json`
with schema name `"stat.classes"` version `"1.0"`.

## Prefixes Covered

The inventory covers **all** Crab prefixes, not only tier-eligible
ones (requirement C1.6):

| Prefix | Tier-eligible |
|--------|--------------|
| `.crab/xorbs/` | Yes |
| `.crab/shards/` | No |
| `.crab/file-index/` | No |
| `.crab/tier/` | No |
| `.crab/audit/` | Never |
| `.crab/tombstones/` | No |
| `.crab/optimize/xorbs/` | No |

## Access-Pattern Input

When `crab-audit` records are present, the cost optimizer loads and
correlates them to refine cold/warm classification per xorb. Without
access data, risk estimates are widened by 25%.

S3 Storage Lens, GCS Monitoring, and Azure Monitor ingestion are
planned follow-ups.
