use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use crate::{AgentError, Result, repository::ReadScope};

const WAL_HEADER_BYTES: u64 = 32;
const FRAME_HEADER_BYTES: u64 = 24;

pub(super) fn verify(database: &Path, journal: &Path, scope: &ReadScope) -> Result<()> {
    let error =
        |message: &str| AgentError::Other(format!("Invalid captured SQLite WAL: {message}"));
    let file = File::open(journal)?;
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(());
    }
    if length < WAL_HEADER_BYTES {
        return Err(error("incomplete header"));
    }
    let mut reader = BufReader::new(file);
    let mut header = [0; WAL_HEADER_BYTES as usize];
    reader.read_exact(&mut header)?;
    let little_endian = match word(&header[..4], false) {
        0x377f_0682 => true,
        0x377f_0683 => false,
        _ => return Err(error("invalid signature")),
    };
    if word(&header[4..8], false) != 3_007_000 {
        return Err(error("unsupported format version"));
    }
    let mut database_header = [0; 20];
    File::open(database)?.read_exact(&mut database_header)?;
    let db_page_size = match u16::from_be_bytes([database_header[16], database_header[17]]) {
        1 => 65_536,
        size => u32::from(size),
    };
    let page_size = word(&header[8..12], false);
    if &database_header[..16] != b"SQLite format 3\0"
        || database_header[18..20] != [2, 2]
        || page_size != db_page_size
        || !(512..=65_536).contains(&page_size)
        || !page_size.is_power_of_two()
    {
        return Err(error("database and WAL headers disagree"));
    }
    let mut checksum = checksum_bytes([0; 2], &header[..24], little_endian);
    if checksum != [word(&header[24..28], false), word(&header[28..32], false)] {
        return Err(error("header checksum differs"));
    }
    let mut frame = [0; FRAME_HEADER_BYTES as usize];
    let mut page = vec![0; page_size as usize];
    let mut remaining = length - WAL_HEADER_BYTES;
    while remaining != 0 {
        scope.check()?;
        let frame_size = FRAME_HEADER_BYTES + u64::from(page_size);
        if remaining < FRAME_HEADER_BYTES {
            return Err(error("incomplete frame header"));
        }
        reader.read_exact(&mut frame)?;
        // WAL reuse can leave pages from the previous salt after the active
        // frames. SQLite ignores that suffix when recovering the current WAL.
        if frame[8..16] != header[16..24] {
            break;
        }
        if remaining < frame_size {
            return Err(error("incomplete frame"));
        }
        if word(&frame[..4], false) == 0 {
            return Err(error("invalid page number"));
        }
        reader.read_exact(&mut page)?;
        checksum = checksum_bytes(checksum, &frame[..8], little_endian);
        checksum = checksum_bytes(checksum, &page, little_endian);
        if checksum != [word(&frame[16..20], false), word(&frame[20..24], false)] {
            return Err(error("frame checksum differs"));
        }
        remaining -= frame_size;
    }
    scope.check()
}

fn word(bytes: &[u8], little_endian: bool) -> u32 {
    let value = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if little_endian {
        u32::from_le_bytes(value)
    } else {
        u32::from_be_bytes(value)
    }
}

fn checksum_bytes(mut checksum: [u32; 2], bytes: &[u8], little_endian: bool) -> [u32; 2] {
    for pair in bytes.chunks_exact(8) {
        checksum[0] = checksum[0]
            .wrapping_add(word(&pair[..4], little_endian))
            .wrapping_add(checksum[1]);
        checksum[1] = checksum[1]
            .wrapping_add(word(&pair[4..], little_endian))
            .wrapping_add(checksum[0]);
    }
    checksum
}
