//! Independent literal-block vectors and a bitwise CRC oracle. These do not
//! call the imported compressor, encoder, or crc-fast implementation.

use crate::{
    CHECKSUM_FLAG, CrabError, Limits, LocalSegment, Position, SegmentInfo, VerifiedPlan, ltx,
};

fn crc(bytes: &[u8]) -> u64 {
    let mut sum = !0u64;
    for byte in bytes {
        sum ^= u64::from(*byte);
        for _ in 0..8 {
            sum = (sum >> 1)
                ^ if sum & 1 != 0 {
                    0xd800_0000_0000_0000
                } else {
                    0
                };
        }
    }
    !sum
}

fn page_sum(pgno: u32, data: &[u8]) -> u64 {
    let mut bytes = pgno.to_be_bytes().to_vec();
    bytes.extend_from_slice(data);
    CHECKSUM_FLAG | crc(&bytes)
}

fn varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        out.push(n as u8 | 128);
        n >>= 7;
    }
    out.push(n as u8);
}

// Deliberately permits invalid order, checksums, and index offsets so tests can
// produce malformed files with otherwise valid outer CRC/digest expectations.
fn fixture(
    pages: &[(u32, Vec<u8>)],
    commit: u32,
    post: u64,
    index_bias: u64,
    legacy: bool,
) -> Vec<u8> {
    use std::io::Write;
    let mut bytes = vec![0; 100];
    bytes[0..4].copy_from_slice(b"LTX1");
    bytes[8..12].copy_from_slice(&512u32.to_be_bytes());
    bytes[12..16].copy_from_slice(&commit.to_be_bytes());
    bytes[16..24].copy_from_slice(&1u64.to_be_bytes());
    bytes[24..32].copy_from_slice(&1u64.to_be_bytes());
    let mut hashed = bytes.clone();
    let mut index = Vec::new();
    for (pgno, data) in pages {
        let offset = bytes.len() as u64;
        let mut header = pgno.to_be_bytes().to_vec();
        header.extend_from_slice(&(if legacy { 0u16 } else { 1u16 }).to_be_bytes());
        let payload = if legacy {
            let info = lz4_flex::frame::FrameInfo::new().content_checksum(true);
            let mut encoder = lz4_flex::frame::FrameEncoder::with_frame_info(info, Vec::new());
            encoder.write_all(data).unwrap();
            encoder.finish().unwrap()
        } else {
            // 512 literals: 15 in the token, then 255 + 242 extension bytes.
            let mut payload = vec![0xf0, 255, 242];
            payload.extend_from_slice(data);
            header.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            payload
        };
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&payload);
        hashed.extend_from_slice(&header);
        hashed.extend_from_slice(data);
        varint(&mut index, u64::from(*pgno));
        varint(&mut index, offset + index_bias);
        varint(&mut index, bytes.len() as u64 - offset);
    }
    index.push(0);
    let mut tail = vec![0; 6];
    tail.extend_from_slice(&index);
    tail.extend_from_slice(&(index.len() as u64).to_be_bytes());
    tail.extend_from_slice(&post.to_be_bytes());
    bytes.extend_from_slice(&tail);
    hashed.extend_from_slice(&tail);
    bytes.extend_from_slice(&(CHECKSUM_FLAG | crc(&hashed)).to_be_bytes());
    bytes
}

#[test]
fn independent_crc_and_both_ltx_page_encodings_match() {
    assert_eq!(crc(b"123456789"), 0xb909_56c7_75a4_1001);
    let data = vec![0x39; 512];
    for legacy in [false, true] {
        let bytes = fixture(&[(1, data.clone())], 1, page_sum(1, &data), 0, legacy);
        let (file, decoded) = ltx::decode_file_with_pages(&bytes).unwrap();
        assert_eq!(decoded, vec![(1, data.clone())]);
        assert_eq!(file.trailer.post_apply_checksum, page_sum(1, &data));
    }
}

#[test]
fn valid_outer_checksums_do_not_hide_bad_page_order_or_index() {
    let one = vec![1; 512];
    let two = vec![2; 512];
    for (pages, commit, bias) in [
        (vec![(1, one.clone()), (1, one.clone())], 2, 0),
        (vec![(2, two.clone()), (1, one.clone())], 2, 0),
        (vec![(1, one.clone())], 2, 0),
        (vec![(1, one.clone())], 1, 1),
        (vec![(2, two)], 1, 0),
    ] {
        let sum = pages.iter().fold(CHECKSUM_FLAG, |sum, (pgno, data)| {
            CHECKSUM_FLAG | (sum ^ page_sum(*pgno, data))
        });
        assert!(ltx::decode_file(&fixture(&pages, commit, sum, bias, false)).is_err());
    }
}

#[test]
fn every_truncated_prefix_is_rejected_without_panicking() {
    let data = vec![8; 512];
    let bytes = fixture(&[(1, data.clone())], 1, page_sum(1, &data), 0, false);
    for end in 0..bytes.len() {
        assert!(ltx::decode_file(&bytes[..end]).is_err());
    }
}

#[test]
fn exact_restore_rejects_checksum_disabled_file() {
    let temp = tempfile::TempDir::new().unwrap();
    let data = vec![0; 512];
    let mut bytes = fixture(&[(1, data.clone())], 1, 0, 0, false);
    bytes[4..8].copy_from_slice(&2u32.to_be_bytes());
    // Recompute the file CRC with the decompressed page, as mandated by LTX.
    let mut hashed = bytes[..110].to_vec();
    hashed.extend_from_slice(&data);
    hashed.extend_from_slice(&bytes[625..bytes.len() - 8]);
    let len = bytes.len();
    bytes[len - 8..].copy_from_slice(&(CHECKSUM_FLAG | crc(&hashed)).to_be_bytes());
    let file = ltx::decode_file(&bytes).unwrap();
    let info = SegmentInfo::from_decoded(&bytes, &file);
    let path = temp.path().join("unchecked.ltx");
    std::fs::write(&path, bytes).unwrap();
    let result = VerifiedPlan::new(
        &[LocalSegment::new(path, info)],
        Position {
            txid: 1,
            checksum: 0,
        },
        Limits::default(),
    );
    assert!(matches!(result, Err(CrabError::LTXCorrupted)));
}

#[test]
fn captured_positions_match_full_database_crc_oracle() {
    let temp = tempfile::TempDir::new().unwrap();
    let mut db = crate::Db::open(&temp.path().join("source.sqlite"), Limits::default()).unwrap();
    let mut segments = Vec::new();
    for round in 0..8 {
        db.transaction(|tx| {
            tx.execute(
                "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, data BLOB)",
                [],
            )?;
            tx.execute("INSERT INTO t(data) VALUES (randomblob(9000))", [])?;
            tx.execute("UPDATE t SET data = randomblob(5000) WHERE id % 2 = 0", [])?;
            Ok(())
        })
        .unwrap();
        let batch = db.capture().unwrap();
        segments.extend(batch.segments);
        let plan = VerifiedPlan::new(&segments, batch.position, Limits::default()).unwrap();
        let path = temp.path().join(format!("restored-{round}.sqlite"));
        crate::restore_exact(&plan, &path).unwrap();
        let image = std::fs::read(path).unwrap();
        let sum = image
            .as_chunks::<4096>()
            .0
            .iter()
            .enumerate()
            .fold(CHECKSUM_FLAG, |sum, (i, page)| {
                CHECKSUM_FLAG | (sum ^ page_sum(i as u32 + 1, page))
            });
        assert_eq!(batch.position.checksum, sum);
    }
}

#[test]
fn altered_delta_predecessor_or_post_state_is_rejected_with_valid_file_crc() {
    let temp = tempfile::TempDir::new().unwrap();
    let before = vec![1; 512];
    let after = vec![2; 512];
    let first = fixture(&[(1, before.clone())], 1, page_sum(1, &before), 0, false);
    let select = |name: &str, bytes: Vec<u8>| {
        let decoded = ltx::decode_file(&bytes).unwrap();
        let info = SegmentInfo::from_decoded(&bytes, &decoded);
        let path = temp.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        LocalSegment::new(path, info)
    };
    let first = select("first.ltx", first);
    for (i, bad_pre) in [true, false].into_iter().enumerate() {
        let post = page_sum(1, &after) ^ if bad_pre { 0 } else { 1 };
        let mut bytes = fixture(&[(1, after.clone())], 1, post, 0, false);
        bytes[16..24].copy_from_slice(&2u64.to_be_bytes());
        bytes[24..32].copy_from_slice(&2u64.to_be_bytes());
        let pre = page_sum(1, &before) ^ u64::from(bad_pre);
        bytes[40..48].copy_from_slice(&pre.to_be_bytes());
        let mut hashed = bytes[..110].to_vec();
        hashed.extend_from_slice(&after);
        hashed.extend_from_slice(&bytes[625..bytes.len() - 8]);
        let len = bytes.len();
        bytes[len - 8..].copy_from_slice(&(CHECKSUM_FLAG | crc(&hashed)).to_be_bytes());
        let delta = select(&format!("delta-{i}.ltx"), bytes);
        let target = delta.info().position();
        let result = VerifiedPlan::new(&[first.clone(), delta], target, Limits::default());
        assert!(matches!(result, Err(CrabError::ChecksumMismatch)));
    }
}

#[test]
fn compressor_round_trips_all_sqlite_page_sizes_and_patterns() {
    let mut compressor = crate::lz4_block::Compressor::default();
    let mut random = 0x6a09e667f3bcc908u64;
    for size in [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536] {
        for pattern in 0..8 {
            let page: Vec<u8> = (0..size)
                .map(|i| {
                    random ^= random << 13;
                    random ^= random >> 7;
                    random ^= random << 17;
                    match pattern {
                        0 => 0,
                        1 => 255,
                        2 => i as u8,
                        3 => (i % 11) as u8,
                        _ => random as u8,
                    }
                })
                .collect();
            let compressed = compressor.compress(&page).unwrap();
            let restored = lz4_flex::block::decompress(&compressed, size).unwrap();
            assert_eq!(restored, page);
        }
    }
}
