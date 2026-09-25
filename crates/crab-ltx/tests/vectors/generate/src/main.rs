//! Writes the external LTX vectors `crates/crab-ltx/tests/vectors/` holds.
//!
//! Every byte comes from the upstream celld encoder pinned in `UPSTREAM.md`, so
//! the Crab reader is proven against an independent implementation rather than
//! against itself. The same inputs are reproducible: the page pattern and the
//! header are fixed here, and the snapshot checksum is computed the way both
//! implementations define it.
//!
//! Run with a target directory outside the repository:
//!
//! ```sh
//! CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ltx-vectors" \
//!   cargo run --release --manifest-path \
//!   crates/crab-ltx/tests/vectors/generate/Cargo.toml -- \
//!   crates/crab-ltx/tests/vectors
//! ```

use celld_ltx::ltx::{self, Header, encode_file, encode_file_v0_5_2};
use celld_ltx::{CHECKSUM_FLAG, TXID};
use std::path::PathBuf;

/// Deterministic page body shared with the Crab vector test.
fn page(page_size: u32, pgno: u32) -> Vec<u8> {
    (0..page_size)
        .map(|index| ((pgno * 37 + index) % 251) as u8)
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: vector-generator <output directory>")?,
    );
    std::fs::create_dir_all(&output)?;

    let cases = [
        ("celld-10cb130-snapshot-block-512.ltx", 512_u32, 3_u32, true),
        ("celld-10cb130-snapshot-frame-512.ltx", 512, 3, false),
        ("celld-10cb130-snapshot-block-4096.ltx", 4096, 4, true),
    ];
    for (name, page_size, commit, block) in cases {
        let pages: Vec<(u32, Vec<u8>)> = (1..=commit)
            .map(|pgno| (pgno, page(page_size, pgno)))
            .collect();
        // A snapshot's post-apply checksum is the checksum-flag seed folded with
        // every page checksum, which is exactly what both decoders recompute.
        let mut post_apply_checksum = CHECKSUM_FLAG;
        for (pgno, data) in &pages {
            post_apply_checksum =
                CHECKSUM_FLAG | (post_apply_checksum ^ ltx::checksum_page(*pgno, data));
        }
        let header = Header {
            version: 3,
            flags: 0,
            page_size,
            commit,
            min_txid: TXID(1),
            max_txid: TXID(1),
            timestamp: 1_700_000_000_000,
            pre_apply_checksum: 0,
            wal_offset: 0,
            wal_size: 0,
            wal_salt1: 0,
            wal_salt2: 0,
            node_id: 0,
        };
        let bytes = if block {
            encode_file_v0_5_2(&header, &pages, post_apply_checksum)
        } else {
            encode_file(&header, &pages, post_apply_checksum)
        }
        .map_err(|error| format!("{name}: {error:?}"))?;
        std::fs::write(output.join(name), &bytes)?;
        println!(
            "{name}: {} bytes, page_size={page_size}, commit={commit}, post={post_apply_checksum:#x}",
            bytes.len()
        );
    }
    write_delta(&output)?;
    Ok(())
}

/// Writes one reference-encoded successor file for the 512-byte snapshot.
///
/// The delta replaces page 2 and publishes the resulting database checksum:
/// the replaced page leaves the rolling checksum and its replacement enters it.
fn write_delta(output: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    const PAGE_SIZE: u32 = 512;
    const COMMIT: u32 = 3;
    let old = page(PAGE_SIZE, 2);
    let next: Vec<u8> = page(PAGE_SIZE, 2)
        .into_iter()
        .map(|byte| 255 - byte)
        .collect();
    let snapshot_pages: Vec<(u32, Vec<u8>)> = (1..=COMMIT)
        .map(|pgno| (pgno, page(PAGE_SIZE, pgno)))
        .collect();
    let mut pre_apply_checksum = CHECKSUM_FLAG;
    for (pgno, data) in &snapshot_pages {
        pre_apply_checksum =
            CHECKSUM_FLAG | (pre_apply_checksum ^ ltx::checksum_page(*pgno, data));
    }
    let post_apply_checksum = CHECKSUM_FLAG
        | (pre_apply_checksum
            ^ ltx::checksum_page(2, &old)
            ^ ltx::checksum_page(2, &next));
    let header = Header {
        version: 3,
        flags: 0,
        page_size: PAGE_SIZE,
        commit: COMMIT,
        min_txid: TXID(2),
        max_txid: TXID(2),
        timestamp: 1_700_000_000_000,
        pre_apply_checksum,
        wal_offset: 0,
        wal_size: 0,
        wal_salt1: 0,
        wal_salt2: 0,
        node_id: 0,
    };
    let name = "celld-10cb130-delta-2-2-512.ltx";
    let bytes = encode_file_v0_5_2(&header, &[(2, next)], post_apply_checksum)
        .map_err(|error| format!("{name}: {error:?}"))?;
    std::fs::write(output.join(name), &bytes)?;
    println!(
        "{name}: {} bytes, pre={pre_apply_checksum:#x}, post={post_apply_checksum:#x}",
        bytes.len()
    );
    Ok(())
}
