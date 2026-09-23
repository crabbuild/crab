# Cell coordination model

This directory contains the bounded TLA+ safety model for the private
coordination kernel in `../src/coordination.rs`. It is a reviewable protocol
model, not a model of SQLite, provider behavior, cryptographic signing, HTTP,
or the primitive schemas.

## Revisions and commands

The model is maintained with the Rust kernel at the current source revision.
The CI workflow records that revision alongside every broad result. The TLC
runner downloads only the official `v1.8.0` artifact named in `toolchain.env`,
verifies its manifest provenance (`Implementation-Title`, vendor, and the
`tlc2/TLC.class` entry), and fails closed on a mismatch. Upstream rebuilds and
re-uploads that asset in place, so the runner logs the exact build revision
instead of pinning mutable bytes; set `CRAB_CELL_TLC_SHA256` to pin an exact
digest where reproducibility matters.

```text
crates/crab-cell-runtime/model/check.sh fast
crates/crab-cell-runtime/model/check.sh negative
crates/crab-cell-runtime/model/check.sh broad
```

`fast` checks the positive bounded state space and the stable-provider liveness
profile. `negative` runs four deliberately broken configurations and requires
the named invariant violation. `broad` uses the same safety and liveness models
with a deeper bound and is scheduled/manual CI evidence.

## Modeled transitions

The actions correspond to the kernel's admission, publication, acknowledgement,
renewal, fence/crash, takeover, and release decisions. The positive configuration
checks single ownership, admission gates, acknowledgement durability, release
ordering, retained recovery obligations, and natural/monotonic epoch and
publication watermarks. The fair drain-completion property is checked by
`CellCoordinationLiveness.cfg`; it intentionally excludes new fence/crash
events so the claim is eventual provider response, not an unbounded-failure
guarantee. The temporal admission/release properties are kept in the
`PROPERTIES` section because TLC distinguishes state predicates from
action formulas. `retained` is the bounded
publication obligation, `published` is the monotonic publication watermark,
and `owner`/`state` represent the single authoritative serving owner. The
mapping and intentional reductions are recorded in [DELTA.md](DELTA.md).

The Rust coordination simulator covers adapter-level schedules that are not
useful in this small model: task cancellation, duplicate or lost completions,
resource pressure, richer membership, and provider failures. Conversely, TLC
enumerates every bounded action interleaving; it does not prove the Rust
adapter or any external service.

## Expected evidence

Positive configurations must finish without an invariant violation and report
their state counts. Negative configurations must identify the configured
invariant by name. A passing model run is protocol-assurance evidence only;
release and Celld comparison claims still require the qualification receipts
described in `../docs/delivery.md`.
