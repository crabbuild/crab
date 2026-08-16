# crab-xet

`crab-xet` owns Crab’s content-addressed data plane: content-defined chunks,
Xorb containers, metadata shards, reconstruction terms, and integrity checks.
It adapts the Xet data structures Crab stores in object storage into reusable
Rust APIs.

## Why it exists

Large files should be uploaded and read as reusable pieces rather than as
whole-file blobs. This crate centralizes the format and hashing rules that make
deduplication safe: the same input chunk has the same identity, every Xorb
placement is verifiable, and reconstruction either covers the complete file or
returns an error.

## Architecture

```text
file bytes
   │
   ▼
content-defined chunks ──► BLAKE3 chunk hashes
   │                              │
   ▼                              ▼
XorbBuilder ───────────────► Xorb bytes + placements
   │                              │
   └──────────────► ShardWriter / ShardReader
                                      │
                                      ▼
                         file chunk sequence + Xorb ranges
                                      │
                                      ▼
                              verified reconstruction
```

The public modules group around that flow:

- `chunker` — optional gearhash chunking (`chunker` feature);
- `hash` and `entropy` — Merkle/BLAKE3 identity and compression heuristics;
- `xorb` — serialized format, builder, parser, compression, and random-access
  chunk verification;
- `shard`, `shard_parse`, and `shard_bloom` — Xet metadata shards and fast
  membership checks;
- `reconstruction` — coalesced `FileTerm`s and complete coverage validation;
- `defrag` and `upload_concurrency` — packing continuity and optional bounded
  upload admission.

## Usage

```rust
#[cfg(feature = "chunker")]
use crab_xet::chunker::GearChunker;

#[cfg(feature = "chunker")]
fn chunk(data: &[u8]) {
    let mut chunker = GearChunker::new();
    let mut chunks = chunker.feed(data);
    if let Some(last) = chunker.finalize() {
        chunks.push(last);
    }
    println!("{} content-defined chunks", chunks.len());
}
```

For a lower-level read path, parse a complete serialized Xorb with
`xorb::parser::XorbParser`, call `verify_payload_digest`, then use
`get_chunk`/`get_chunk_range`. For writes, feed Xet `Chunk` values to
`xorb::builder::XorbBuilder` and upload each `XorbResult` through the owning
storage or staging layer.

## Feature flags and invariants

- `chunker` adds the Xet gearhash chunker and `GearChunker`.
- `upload-concurrency` adds Xet client/runtime admission helpers.
- Default features remain empty so payload-only users do not inherit runtimes
  or network clients.

Hashes cover the exact content or serialized payload they name. Shard terms
must cover every file chunk in order; callers should use
`build_file_terms` and `validate_term_coverage` rather than reconstructing
ranges themselves.
