//! Diagnostic: dump MDBFileInfo segments for a file in a shard.
//!
//! Usage: cargo run --release --bin dump_shard_file -- <shard-file> <file-hash>
//!
//! File hash is in MerkleHash::hex() style (LE u64 per group, same as the
//! on-disk file-index object name).

use std::env;
use std::fs;
use std::process::ExitCode;

use bytes::Bytes;
use crab_xet::hash::MerkleHash;
use crab_xet::shard::ShardReader;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: dump_shard_file <shard-path> <file-hash-merklehash-hex>");
        return ExitCode::from(2);
    }
    let shard_path = &args[1];
    let file_hex = &args[2];

    let data = match fs::read(shard_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("read shard failed: {e}");
            return ExitCode::from(2);
        }
    };
    let file_hash = match MerkleHash::from_hex(file_hex) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("file hash parse failed: {e:?}");
            return ExitCode::from(2);
        }
    };

    let reader = ShardReader::from_bytes(Bytes::from(data), MerkleHash::default());

    let fi = match reader.get_file_info(&file_hash) {
        Ok(Some(info)) => info,
        Ok(None) => {
            println!("file-hash NOT in shard (get_file_info returned None)");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("get_file_info failed: {e}");
            return ExitCode::from(1);
        }
    };

    println!("file: {}", fi.metadata.file_hash.hex());
    println!("num segments: {}", fi.segments.len());

    let mut total_unpacked: u64 = 0;
    let mut total_chunks: u32 = 0;
    let mut file_pos: u32 = 0;
    for (i, seg) in fi.segments.iter().enumerate() {
        let chunks = seg.chunk_index_end.saturating_sub(seg.chunk_index_start);
        total_chunks += chunks;
        total_unpacked += seg.unpacked_segment_bytes as u64;
        println!(
            "  [{i:04}] xorb={}… range=[{},{}) chunks={} unpacked={} file_pos={}",
            &seg.xorb_hash.hex()[..16],
            seg.chunk_index_start,
            seg.chunk_index_end,
            chunks,
            seg.unpacked_segment_bytes,
            file_pos,
        );
        file_pos += chunks;
    }

    println!();
    println!("TOTAL chunks (sum of segment ranges): {total_chunks}");
    println!("TOTAL unpacked_segment_bytes:         {total_unpacked}");

    ExitCode::from(0)
}
