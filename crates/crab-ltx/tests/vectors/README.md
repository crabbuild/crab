# External LTX vectors

These files were written by upstream implementations, not by `crab-ltx`. They
are the independent half of the format qualification: `crab-ltx` must decode
them, restore the exact database image they encode, and (for the sized-block
representation) reproduce them byte for byte.

## Provenance

| Field | Value |
| --- | --- |
| Producers | `celld-ltx` at `10cb1303dac710dcb3b557e318e08c855261f68b` (the revision `UPSTREAM.md` pins), and `superfly/ltx` v0.5.2 — the Go reference library Litestream v0.5.17 uses |
| Encoders | `ltx::encode_file_v0_5_2` (sized block), `ltx::encode_file` (legacy LZ4 frame), and the Go `ltx.Encoder` |
| Generators | `generate/` (Rust, celld) and `generate/go/` (Go, superfly/ltx) |
| Pattern | page `p` byte `i` is `(p * 37 + i) % 251` |
| Header | version 3, `min_txid = max_txid = 1`, timestamp `1_700_000_000_000`, zero WAL fields |

| File | page size | pages | encoding |
| --- | --- | --- | --- |
| `celld-10cb130-snapshot-block-512.ltx` | 512 | 3 | sized block |
| `celld-10cb130-snapshot-frame-512.ltx` | 512 | 3 | legacy LZ4 frame |
| `celld-10cb130-snapshot-block-4096.ltx` | 4096 | 4 | sized block |
| `superfly-ltx-v0.5.2-snapshot-block-512.ltx` | 512 | 3 | sized block, Go reference writer |
| `celld-10cb130-delta-2-2-512.ltx` | 512 | 1 page (2) | sized block successor, continues the 512 snapshot |
| `superfly-ltx-v0.5.2-delta-2-2-512.ltx` | 512 | 1 page (2) | sized block successor, Go reference writer |

The reference writer and the Celld port produce byte-identical 512-byte files
(`sha256 f43c185e114e793fcf427f6f98002bfeb24dd8bfec8ca5b2c75c248f2598664c`),
which `src/format_tests.rs::independent_producers_agree_byte_for_byte` pins. The
byte-equality assertion against this crate's writer is therefore also the
reverse-direction proof: bytes every reader accepts are bytes this crate emits.
The two successor files are identical as well (`sha256 52144b14…`), and
`external_delta_chain_restores_and_matches_our_writer` restores the snapshot plus
successor chain to the exact image with the replaced page.

The snapshot post-apply checksum is `CHECKSUM_FLAG | (checksum ^ page_checksum)`
folded over the pages, which is the recurrence both implementations decode.

## Regenerate

The generator is its own workspace so `cargo test -p crab-ltx` never fetches or
builds upstream sources:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ltx-vectors" \
  cargo run --release --manifest-path \
  crates/crab-ltx/tests/vectors/generate/Cargo.toml -- \
  crates/crab-ltx/tests/vectors
```

`src/format_tests.rs::external_snapshots_decode_restore_and_match_our_writer`
consumes these files, and `tests/ltx/vectors.rs` reuses them as the seed corpus
for the decoder-panic replay that mirrors `fuzz/`.

The Go vector regenerates with:

```sh
cd crates/crab-ltx/tests/vectors/generate/go
go run . ../..
```

Adding a vector: extend the generator's case list, regenerate, and record the
new row above. Changing a vector's bytes without regenerating fails the
byte-equality assertion, which is the point.
