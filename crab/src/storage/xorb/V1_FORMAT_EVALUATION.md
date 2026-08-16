# Evaluation: Adopting xet-core's V1 Xorb Format (`SerializedXorbObject`)

## Summary

**Recommendation: Do NOT adopt `SerializedXorbObject` directly. Instead, selectively
adopt V1 metadata concepts (boundary offset tables, jump pointers) into crab's
existing format in a future iteration.**

The two formats are structurally incompatible — they use different binary layouts,
different per-chunk framing, and different footer structures. Adopting V1 wholesale
would require rewriting both builder and parser, changing the on-disk format for all
xorbs crab produces, and adding backward-compatibility shims. The cost outweighs
the benefit for the current XF4 milestone.

---

## Format Comparison

### Crab Current Format

```
[compressed_chunk_0][compressed_chunk_1]...[compressed_chunk_N]
[chunk_meta_0: hash(32) + offset(4) + compressed_len(4) + uncompressed_len(4)]...
[footer: num_chunks(4) + meta_offset(8) + magic(4)]
```

- **Per-chunk data**: Raw compressed bytes, no per-chunk header.
- **Metadata section**: Flat array of 44-byte entries (hash + offset + compressed_len + uncompressed_len).
- **Footer**: 16 bytes — num_chunks(4) + meta_offset(8) + magic "XORB"(4).
- **Chunk access**: O(1) by index — metadata is a fixed-stride array, so `meta[i]` is at
  `meta_offset + i * 44`. The parser currently reads all metadata into a `Vec<ChunkMeta>`
  upfront, but the format itself supports O(1) random access.
- **Range GET byte range**: O(1) — each `ChunkMeta` stores the byte offset and compressed
  length directly. `(meta[start].offset, meta[end-1].offset + meta[end-1].compressed_len)`.
- **Compression**: Currently hardcoded zstd-3 for all chunks. No per-chunk compression
  scheme field in the metadata (the scheme is implicit).
- **Integrity**: Hash verification per chunk on decompression. No footer-level range hash.

### xet-core V1 Format (`SerializedXorbObject` / `XorbObjectInfoV1`)

```
[chunk_header_0(8) + compressed_data_0][chunk_header_1(8) + compressed_data_1]...
[XorbObjectInfoV1 footer]
[info_length(4)]
```

- **Per-chunk data**: Each chunk has an 8-byte `XorbChunkHeader` prefix:
  `version(1) + compressed_len(3) + compression_scheme(1) + uncompressed_len(3)`.
- **Footer** (`XorbObjectInfoV1`): Multi-section structure:
  - Calf section: ident "XETBLOB"(7) + version(1) + xorb_hash(32)
  - Hash section: ident "XBLBHSH"(7) + version(1) + num_chunks(4) + chunk_hashes(32 * N)
  - Boundary section: ident "XBLBBND"(7) + version(1) + num_chunks(4) +
    chunk_boundary_offsets(4 * N) + unpacked_chunk_offsets(4 * N)
  - Fixed tail: num_chunks(4) + hashes_section_offset(4) + boundary_section_offset(4) + buffer(16)
  - Trailing: info_length(4)
- **Chunk access**: O(1) via `chunk_boundary_offsets` — same principle as crab, different encoding.
- **Range GET byte range**: O(1) via `get_byte_offset(start, end)` using `chunk_boundary_offsets`.
  Also has `unpacked_chunk_offsets` for uncompressed byte ranges.
- **Compression**: Per-chunk via `XorbChunkHeader.compression_scheme`. Supports None, LZ4,
  BG4+LZ4, Auto (resolved at serialization time). Note: **no zstd support** — V1 uses LZ4.
- **Integrity**: `validate_xorb_object()` walks all chunks, recomputes hashes, verifies
  boundaries. `generate_chunk_range_hash()` for range-level integrity.

---

## Key Differences

| Aspect | Crab | xet-core V1 |
|--------|--------|-------------|
| Per-chunk header | None (raw compressed bytes) | 8-byte `XorbChunkHeader` |
| Compression | zstd-3 (implicit, uniform) | LZ4/BG4+LZ4/None (per-chunk, in header) |
| Footer magic | "XORB" (4 bytes) | "XETBLOB" (7 bytes) |
| Footer structure | Flat: meta entries + 16-byte footer | Multi-section with jump pointers |
| Metadata per chunk | 44 bytes (hash+offset+comp_len+uncomp_len) | Split across sections (hash in one, boundaries in another) |
| Uncompressed offsets | Stored per-chunk in metadata | Separate `unpacked_chunk_offsets` array |
| Range hash | Not present | `generate_chunk_range_hash()` |
| Hash computation | `xorb_hash(&[(hash, uncompressed_len)])` | Same `xorb_hash` function |
| Dependency on xet_config | None | `SerializedXorbObject::from_xorb()` calls `xet_config()` |

---

## Accessibility from `xet_core_structures`

`SerializedXorbObject`, `XorbObjectInfoV1`, `XorbObject`, `XorbChunkHeader`, and
`CompressionScheme` are all `pub` and accessible from `xet_core_structures::xorb_object`.
The dependency already exists in crab's `Cargo.toml`.

However, `SerializedXorbObject::from_xorb()` requires a `RawXorbData` input, which is
xet-core's internal representation of an uncompressed xorb (with `xorb_info`, `data: Vec<Bytes>`,
`file_boundaries`). Crab's `XorbBuilder` works with individual `Chunk` objects pushed
one at a time — there's no `RawXorbData` equivalent. Adapting to `RawXorbData` would
require buffering all uncompressed chunk data, which defeats the streaming design.

Additionally, `from_xorb()` calls `xet_config()` for compression policy, which is
xet-core's global config singleton — crab uses its own config system.

---

## What V1 Offers That Crab Lacks

1. **`unpacked_chunk_offsets`**: Cumulative uncompressed byte offsets per chunk. Enables
   O(1) computation of "what byte range in the uncompressed content does chunk range
   [i, j) cover?" — useful for Range GET planning on the hydrate path. Crab could
   add this to its own metadata section (4 bytes per chunk) without adopting V1.

2. **Per-chunk compression scheme in header**: V1 embeds the scheme in each chunk's
   8-byte header, making mixed-compression xorbs self-describing. Crab's current
   format has no per-chunk scheme field — task 3.4 (XF1) will need to add one.
   This can be done by extending crab's metadata entries rather than adopting V1's
   chunk header approach.

3. **Section jump pointers**: V1's footer has `hashes_section_offset_from_end` and
   `boundary_section_offset_from_end`, allowing a reader to seek directly to the
   boundary section without parsing hashes. Useful for partial footer reads. Crab's
   flat metadata array doesn't need this — the entire metadata section is a single
   fixed-stride array that's already efficient to parse.

4. **Range hash generation**: `generate_chunk_range_hash()` produces a hash over a
   chunk range for integrity verification of partial xorb reads. Nice to have but
   not required for XF4.

---

## Why NOT to Adopt V1 Directly

1. **Binary incompatibility**: V1 uses per-chunk 8-byte headers inline with data.
   Crab stores raw compressed bytes with metadata at the end. These are fundamentally
   different wire formats. Adopting V1 means every xorb crab produces would be in
   V1 format, and the parser would need to handle both old (crab) and new (V1) formats.

2. **No zstd in V1**: xet-core V1 uses LZ4 (and BG4+LZ4). Crab uses zstd-3.
   Switching compression algorithms is a separate decision with performance implications
   (zstd has better ratios, LZ4 has better speed). The XF1 task already plans to add
   adaptive compression — this should be done on crab's terms, not forced by V1 adoption.

3. **`RawXorbData` mismatch**: `SerializedXorbObject::from_xorb_with_compression()`
   expects a fully-buffered `RawXorbData`. Crab's builder streams chunks one at a time
   and compresses incrementally. Adapting to `RawXorbData` would require buffering all
   uncompressed data (~64 MiB) before serialization, adding memory pressure.

4. **`xet_config()` coupling**: V1 serialization reads compression policy from xet-core's
   global config. Crab has its own config system. Using `from_xorb_with_compression()`
   with an explicit scheme avoids this, but it's still an awkward API boundary.

5. **Crab's format is already O(1)**: The task description mentions "crab currently
   scans linearly" for chunk access. This is a parser implementation detail, not a format
   limitation. The metadata section is a fixed-stride array — O(1) access by index is
   trivial and already works (see `chunk_meta(index)` which indexes into `self.chunks[index]`).
   The "linear scan" is only during initial parse (reading all metadata entries), which
   is unavoidable in any format.

---

## Recommended Path Forward

1. **Keep crab's format for XF4** — the current format works, placement info is already
   implemented, and the format supports everything XF4 needs.

2. **For XF1 (compression-aware packing)**: Extend crab's chunk metadata entry with a
   1-byte compression scheme field (45 bytes per entry instead of 44). This is simpler
   than adopting V1's per-chunk header approach and keeps the metadata-at-end layout.

3. **Consider adding `unpacked_chunk_offsets`** to crab's footer in a future iteration
   if Range GET optimization on the hydrate path becomes a priority. This is a 4-byte-per-chunk
   addition to the metadata section — straightforward without V1 adoption.

4. **If full V1 compatibility is ever needed** (e.g., crab needs to read xorbs produced
   by xet-core, or vice versa), implement a separate `V1Parser` that uses
   `XorbObject::deserialize()` from `xet_core_structures`. This is a read-path addition,
   not a write-path change.
