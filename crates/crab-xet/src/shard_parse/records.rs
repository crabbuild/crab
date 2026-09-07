use std::io::Read;

use bytes::Bytes;
use xet_core_structures::{
    CoreError,
    metadata_shard::{
        file_structs::{FileDataSequenceHeader, MDBFileInfoView},
        shard_file::MDB_FILE_INFO_ENTRY_SIZE,
        xorb_structs::{MDBXorbInfoView, XorbChunkSequenceEntry, XorbChunkSequenceHeader},
    },
};

use super::MAX_SHARD_SIZE_BYTES;

type Result<T> = std::result::Result<T, CoreError>;

// The upstream streaming helpers reserve the declared record size before reading
// it. Read incrementally here so a truncated record cannot force that allocation;
// keep upstream header codecs and views as the wire-format authority.
fn read_record(reader: &mut impl Read, mut header: Vec<u8>, payload_bytes: u64) -> Result<Bytes> {
    if payload_bytes > (MAX_SHARD_SIZE_BYTES - header.len()) as u64 {
        return Err(CoreError::InvalidShard(
            "record exceeds shard byte limit".to_owned(),
        ));
    }
    let read = std::io::copy(&mut reader.take(payload_bytes), &mut header)?;
    if read != payload_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "truncated shard record",
        )
        .into());
    }
    Ok(Bytes::from(header))
}

pub(super) fn process_shard_file_info_section<R: Read, F>(
    reader: &mut R,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(MDBFileInfoView) -> Result<()>,
{
    loop {
        let header = FileDataSequenceHeader::deserialize(reader)?;
        if header.is_bookend() {
            return Ok(());
        }
        let entries = u64::from(header.num_entries);
        let records = entries * if header.contains_verification() { 2 } else { 1 }
            + u64::from(header.contains_metadata_ext());
        let mut bytes = Vec::new();
        header.serialize(&mut bytes)?;
        let bytes = read_record(reader, bytes, records * MDB_FILE_INFO_ENTRY_SIZE as u64)?;
        visit(MDBFileInfoView::from_data_and_header(header, bytes)?)?;
    }
}

pub(super) fn process_shard_xorb_info_section<R: Read, F>(
    reader: &mut R,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(MDBXorbInfoView) -> Result<()>,
{
    loop {
        let header = XorbChunkSequenceHeader::deserialize(reader)?;
        if header.is_bookend() {
            return Ok(());
        }
        let mut bytes = Vec::new();
        header.serialize(&mut bytes)?;
        let bytes = read_record(
            reader,
            bytes,
            u64::from(header.num_entries) * size_of::<XorbChunkSequenceEntry>() as u64,
        )?;
        visit(MDBXorbInfoView::from_data_and_header(header, bytes)?)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_record_flags_match_upstream_layout() {
        for (verification, metadata_ext) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut bytes = Vec::new();
            FileDataSequenceHeader::new(
                MerkleHash::from([1u64; 4]),
                2u32,
                verification,
                metadata_ext,
            )
            .serialize(&mut bytes)
            .unwrap();
            let records = 2 + usize::from(verification) * 2 + usize::from(metadata_ext);
            bytes.resize(bytes.len() + records * MDB_FILE_INFO_ENTRY_SIZE, 0);
            FileDataSequenceHeader::bookend()
                .serialize(&mut bytes)
                .unwrap();
            let mut seen = Vec::new();
            process_shard_file_info_section(&mut Cursor::new(bytes), |view| {
                seen.push(view.num_entries());
                Ok(())
            })
            .unwrap();
            assert_eq!(seen, vec![2]);
        }
    }
    use crate::hash::MerkleHash;
    use std::io::Cursor;

    #[test]
    fn rejects_oversized_records_before_reading_payloads() {
        let mut file = Vec::new();
        FileDataSequenceHeader::new(MerkleHash::from([1u64; 4]), u32::MAX, true, true)
            .serialize(&mut file)
            .unwrap();
        let mut xorb = Vec::new();
        XorbChunkSequenceHeader::new(MerkleHash::from([1u64; 4]), u32::MAX, 0u32)
            .serialize(&mut xorb)
            .unwrap();
        for result in [
            process_shard_file_info_section(&mut Cursor::new(file), |_| Ok(())),
            process_shard_xorb_info_section(&mut Cursor::new(xorb), |_| Ok(())),
        ] {
            assert!(
                matches!(result, Err(CoreError::InvalidShard(message)) if message == "record exceeds shard byte limit")
            );
        }
    }

    #[test]
    fn truncated_record_preserves_io_error() {
        let error =
            read_record(&mut Cursor::new([1u8; 8]), vec![0; 48], 32 * 1024 * 1024).unwrap_err();
        assert!(
            matches!(error, CoreError::Io(source) if source.kind() == std::io::ErrorKind::UnexpectedEof)
        );
    }
}
