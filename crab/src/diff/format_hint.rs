//! Format-aware annotations: extensible two-phase `FormatHint` trait.
//!
//! Recognized file formats (safetensors, parquet) get lightweight semantic
//! annotations by downloading only the header/footer chunk(s) and mapping
//! changed byte ranges to domain structures (tensor names, row groups).
//!
//! The two-phase design separates "what data do I need?" from "what do I
//! do with it?", letting the diff engine batch chunk downloads across all
//! hints before any parsing happens.

use bytes::Bytes;

use crate::diff::formatter::format_size;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which version of the file a chunk request belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileVersion {
    Old,
    New,
}

/// Describes a single chunk the format hint needs downloaded.
#[derive(Debug, Clone)]
pub struct ChunkRequest {
    /// Which version of the file this chunk belongs to.
    pub version: FileVersion,
    /// Segment index within the file's reconstruction terms.
    pub segment_index: usize,
    /// Human-readable purpose (e.g., "safetensors JSON header").
    pub purpose: String,
}

/// Trait for file-format-specific diff annotations.
///
/// Implementations are stateless — all state flows through method arguments.
/// Adding a new file format requires implementing this trait and registering
/// it in [`detect_format_hint`].
pub trait FormatHint: Send + Sync {
    /// Phase 1: declare which chunks are needed for annotation.
    fn required_chunks(&self, file_size: u64, num_segments: usize) -> Vec<ChunkRequest>;

    /// Phase 2: produce annotations from downloaded chunk data.
    ///
    /// `chunk_data` contains the bytes for each [`ChunkRequest`] from phase 1,
    /// in the same order. `changed_ranges` are `(byte_offset, byte_length)`
    /// pairs from the chunk comparator.
    ///
    /// Return an empty vec on parse failure — never return an error.
    fn annotate(&self, chunk_data: &[Bytes], changed_ranges: &[(u64, u64)]) -> Vec<String>;

    /// Human-readable name for this format (e.g., "safetensors", "parquet").
    fn format_name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// detect_format_hint — extension-based dispatch
// ---------------------------------------------------------------------------

/// Detect file format from extension and return the appropriate hint.
///
/// Returns `None` for unrecognized extensions — the diff engine skips
/// annotation entirely in that case.
pub fn detect_format_hint(path: &str) -> Option<Box<dyn FormatHint>> {
    if path.ends_with(".safetensors") {
        Some(Box::new(SafetensorsHint))
    } else if path.ends_with(".parquet") {
        Some(Box::new(ParquetHint))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// SafetensorsHint
// ---------------------------------------------------------------------------

/// Annotates safetensors files by parsing the JSON header to map changed
/// byte ranges to tensor names.
///
/// Header format: first 8 bytes = u64 LE header length, then JSON of that
/// length containing tensor metadata `{ name: { dtype, shape, data_offsets: [start, end] } }`.
pub struct SafetensorsHint;

impl FormatHint for SafetensorsHint {
    fn required_chunks(&self, _file_size: u64, num_segments: usize) -> Vec<ChunkRequest> {
        if num_segments == 0 {
            return Vec::new();
        }
        // Request the first segment from both versions — it contains the
        // JSON header with tensor name → byte offset map.
        vec![
            ChunkRequest {
                version: FileVersion::Old,
                segment_index: 0,
                purpose: "safetensors JSON header".into(),
            },
            ChunkRequest {
                version: FileVersion::New,
                segment_index: 0,
                purpose: "safetensors JSON header".into(),
            },
        ]
    }

    fn annotate(&self, chunk_data: &[Bytes], changed_ranges: &[(u64, u64)]) -> Vec<String> {
        if chunk_data.is_empty() || changed_ranges.is_empty() {
            return Vec::new();
        }
        // Try to parse the new version's header (index 1 if available, else 0).
        let header_bytes = if chunk_data.len() > 1 {
            &chunk_data[1]
        } else {
            &chunk_data[0]
        };
        let Some(tensors) = parse_safetensors_header(header_bytes) else {
            return Vec::new();
        };
        annotate_safetensors(&tensors, changed_ranges)
    }

    fn format_name(&self) -> &'static str {
        "safetensors"
    }
}

/// Parsed tensor entry: name → (data_start, data_end) in the file.
/// Offsets are relative to the start of the data section (after the header).
struct TensorEntry {
    name: String,
    /// Absolute byte offset of the tensor data start within the file.
    start: u64,
    /// Absolute byte offset of the tensor data end within the file.
    end: u64,
}

/// Parse the safetensors JSON header from raw chunk bytes.
///
/// Returns a sorted vec of `TensorEntry` with absolute file offsets, or
/// `None` on any parse failure.
fn parse_safetensors_header(data: &[u8]) -> Option<Vec<TensorEntry>> {
    if data.len() < 8 {
        return None;
    }
    let header_len = u64::from_le_bytes(data[..8].try_into().ok()?) as usize;
    let header_end = 8 + header_len;
    if data.len() < header_end {
        return None;
    }
    let header_json: serde_json::Value = serde_json::from_slice(&data[8..header_end]).ok()?;
    let obj = header_json.as_object()?;

    // The data section starts right after the header.
    let data_offset = header_end as u64;

    let mut entries = Vec::new();
    for (name, value) in obj {
        // Skip the special "__metadata__" key.
        if name == "__metadata__" {
            continue;
        }
        let tensor_obj = value.as_object()?;
        let offsets = tensor_obj.get("data_offsets")?.as_array()?;
        if offsets.len() != 2 {
            return None;
        }
        let start = offsets[0].as_u64()?;
        let end = offsets[1].as_u64()?;
        entries.push(TensorEntry {
            name: name.clone(),
            start: data_offset + start,
            end: data_offset + end,
        });
    }
    entries.sort_by_key(|e| e.start);
    Some(entries)
}

/// Intersect tensor byte ranges with changed ranges and produce annotations.
fn annotate_safetensors(tensors: &[TensorEntry], changed_ranges: &[(u64, u64)]) -> Vec<String> {
    let mut annotations = Vec::new();
    for tensor in tensors {
        let tensor_size = tensor.end.saturating_sub(tensor.start);
        let mut overlap_bytes: u64 = 0;
        for &(offset, length) in changed_ranges {
            let change_end = offset + length;
            // Check overlap between [tensor.start, tensor.end) and [offset, change_end).
            let overlap_start = tensor.start.max(offset);
            let overlap_end = tensor.end.min(change_end);
            if overlap_start < overlap_end {
                overlap_bytes += overlap_end - overlap_start;
            }
        }
        if overlap_bytes > 0 {
            let size_str = format_size(tensor_size);
            annotations.push(format!("tensor {}: {size_str} modified", tensor.name));
        }
    }
    annotations
}

// ---------------------------------------------------------------------------
// ParquetHint
// ---------------------------------------------------------------------------

/// Annotates parquet files by parsing the Thrift-encoded footer to map
/// changed byte ranges to row group indices and sizes.
///
/// Footer format: last 4 bytes = magic "PAR1", preceding 4 bytes = u32 LE
/// footer length, then the Thrift-encoded metadata of that length.
pub struct ParquetHint;

impl FormatHint for ParquetHint {
    fn required_chunks(&self, _file_size: u64, num_segments: usize) -> Vec<ChunkRequest> {
        if num_segments == 0 {
            return Vec::new();
        }
        let last = num_segments - 1;
        // Request the last segment from both versions — it contains the
        // Thrift-encoded footer with row group metadata.
        vec![
            ChunkRequest {
                version: FileVersion::Old,
                segment_index: last,
                purpose: "parquet footer".into(),
            },
            ChunkRequest {
                version: FileVersion::New,
                segment_index: last,
                purpose: "parquet footer".into(),
            },
        ]
    }

    fn annotate(&self, chunk_data: &[Bytes], changed_ranges: &[(u64, u64)]) -> Vec<String> {
        if chunk_data.is_empty() || changed_ranges.is_empty() {
            return Vec::new();
        }
        // Try to parse the new version's footer (index 1 if available, else 0).
        let footer_bytes = if chunk_data.len() > 1 {
            &chunk_data[1]
        } else {
            &chunk_data[0]
        };
        let Some(row_groups) = parse_parquet_footer(footer_bytes) else {
            return Vec::new();
        };
        annotate_parquet(&row_groups, changed_ranges)
    }

    fn format_name(&self) -> &'static str {
        "parquet"
    }
}

/// Parsed row group: index, file offset, and total byte size.
struct RowGroupInfo {
    index: usize,
    /// File offset where this row group starts.
    offset: u64,
    /// Total byte size of the row group.
    total_byte_size: u64,
}

/// Parse the parquet footer from raw chunk bytes using the `parquet` crate.
///
/// Returns a vec of `RowGroupInfo`, or `None` on any parse failure.
fn parse_parquet_footer(data: &[u8]) -> Option<Vec<RowGroupInfo>> {
    // Parquet footer: last 4 bytes = "PAR1", preceding 4 bytes = u32 LE footer length.
    if data.len() < 8 {
        return None;
    }
    let magic = &data[data.len() - 4..];
    if magic != b"PAR1" {
        return None;
    }
    let footer_len_bytes: [u8; 4] = data[data.len() - 8..data.len() - 4].try_into().ok()?;
    let footer_len = u32::from_le_bytes(footer_len_bytes) as usize;

    // The Thrift-encoded metadata sits just before the 8-byte tail.
    let footer_start = data.len().checked_sub(8 + footer_len)?;
    let thrift_bytes = &data[footer_start..data.len() - 8];

    let metadata =
        parquet::file::metadata::ParquetMetaDataReader::decode_metadata(thrift_bytes).ok()?;

    let mut row_groups = Vec::new();
    for (i, rg) in metadata.row_groups().iter().enumerate() {
        // file_offset may not be set; fall back to the first column chunk's offset.
        let offset = rg.file_offset().unwrap_or_else(|| {
            rg.columns().first().map_or(0, |c| {
                // dictionary_page_offset or data_page_offset, whichever is smaller.
                let data_off = c.data_page_offset();
                let dict_off = c.dictionary_page_offset();
                match dict_off {
                    Some(d) if d < data_off => d,
                    _ => data_off,
                }
            })
        }) as u64;

        row_groups.push(RowGroupInfo {
            index: i,
            offset,
            total_byte_size: rg.total_byte_size() as u64,
        });
    }
    Some(row_groups)
}

/// Intersect row group byte ranges with changed ranges and produce annotations.
fn annotate_parquet(row_groups: &[RowGroupInfo], changed_ranges: &[(u64, u64)]) -> Vec<String> {
    let mut annotations = Vec::new();
    for rg in row_groups {
        let rg_end = rg.offset + rg.total_byte_size;
        let mut changed_chunk_count = 0u64;
        for &(offset, length) in changed_ranges {
            let change_end = offset + length;
            // Check overlap between [rg.offset, rg_end) and [offset, change_end).
            let overlap_start = rg.offset.max(offset);
            let overlap_end = rg_end.min(change_end);
            if overlap_start < overlap_end {
                changed_chunk_count += 1;
            }
        }
        if changed_chunk_count > 0 {
            let size_str = format_size(rg.total_byte_size);
            annotations.push(format!(
                "row_group[{}]: {size_str}, {} chunks changed",
                rg.index, changed_chunk_count,
            ));
        }
    }
    annotations
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_safetensors() {
        assert!(detect_format_hint("model.safetensors").is_some());
        assert_eq!(
            detect_format_hint("model.safetensors")
                .as_ref()
                .map(|h| h.format_name()),
            Some("safetensors"),
        );
    }

    #[test]
    fn detect_parquet() {
        assert!(detect_format_hint("data.parquet").is_some());
        assert_eq!(
            detect_format_hint("data.parquet")
                .as_ref()
                .map(|h| h.format_name()),
            Some("parquet"),
        );
    }

    #[test]
    fn detect_unknown_returns_none() {
        assert!(detect_format_hint("readme.md").is_none());
        assert!(detect_format_hint("model.bin").is_none());
        assert!(detect_format_hint("data.csv").is_none());
    }

    #[test]
    fn safetensors_required_chunks_requests_first_segment() {
        let hint = SafetensorsHint;
        let chunks = hint.required_chunks(1_000_000, 10);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].version, FileVersion::Old);
        assert_eq!(chunks[0].segment_index, 0);
        assert_eq!(chunks[1].version, FileVersion::New);
        assert_eq!(chunks[1].segment_index, 0);
    }

    #[test]
    fn safetensors_required_chunks_empty_for_zero_segments() {
        let hint = SafetensorsHint;
        assert!(hint.required_chunks(1_000_000, 0).is_empty());
    }

    #[test]
    fn parquet_required_chunks_requests_last_segment() {
        let hint = ParquetHint;
        let chunks = hint.required_chunks(1_000_000, 10);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].version, FileVersion::Old);
        assert_eq!(chunks[0].segment_index, 9);
        assert_eq!(chunks[1].version, FileVersion::New);
        assert_eq!(chunks[1].segment_index, 9);
    }

    #[test]
    fn parquet_required_chunks_empty_for_zero_segments() {
        let hint = ParquetHint;
        assert!(hint.required_chunks(1_000_000, 0).is_empty());
    }

    #[test]
    fn safetensors_annotate_empty_on_no_data() {
        let hint = SafetensorsHint;
        assert!(hint.annotate(&[], &[(0, 100)]).is_empty());
    }

    #[test]
    fn safetensors_annotate_empty_on_no_changes() {
        let hint = SafetensorsHint;
        let header = build_safetensors_header(&[("weight", 0, 1024)]);
        assert!(hint.annotate(&[header], &[]).is_empty());
    }

    #[test]
    fn safetensors_annotate_with_overlapping_tensor() {
        let hint = SafetensorsHint;
        // Build a header with one tensor at data offsets [0, 1024].
        let header = build_safetensors_header(&[("layer.weight", 0, 1024)]);
        let header_len = {
            let hl = u64::from_le_bytes(header[..8].try_into().unwrap()) as u64;
            8 + hl
        };
        // Changed range overlaps with the tensor data region.
        let annotations = hint.annotate(&[header], &[(header_len, 512)]);
        assert_eq!(annotations.len(), 1);
        assert!(annotations[0].contains("layer.weight"));
        assert!(annotations[0].contains("modified"));
    }

    #[test]
    fn safetensors_annotate_no_overlap() {
        let hint = SafetensorsHint;
        let header = build_safetensors_header(&[("layer.weight", 0, 1024)]);
        let header_len = {
            let hl = u64::from_le_bytes(header[..8].try_into().unwrap()) as u64;
            8 + hl
        };
        // Changed range is entirely after the tensor data.
        let annotations = hint.annotate(&[header], &[(header_len + 2048, 100)]);
        assert!(annotations.is_empty());
    }

    #[test]
    fn safetensors_graceful_on_malformed_data() {
        let hint = SafetensorsHint;
        let garbage = Bytes::from(vec![0u8; 4]);
        assert!(hint.annotate(&[garbage], &[(0, 100)]).is_empty());
    }

    #[test]
    fn format_name_values() {
        assert_eq!(SafetensorsHint.format_name(), "safetensors");
        assert_eq!(ParquetHint.format_name(), "parquet");
    }

    /// Helper: build a minimal safetensors header blob.
    fn build_safetensors_header(tensors: &[(&str, u64, u64)]) -> Bytes {
        let mut map = serde_json::Map::new();
        for &(name, start, end) in tensors {
            let mut tensor = serde_json::Map::new();
            tensor.insert("dtype".into(), serde_json::Value::String("F32".into()));
            tensor.insert(
                "shape".into(),
                serde_json::Value::Array(vec![serde_json::json!(256)]),
            );
            tensor.insert(
                "data_offsets".into(),
                serde_json::Value::Array(vec![serde_json::json!(start), serde_json::json!(end)]),
            );
            map.insert(name.to_string(), serde_json::Value::Object(tensor));
        }
        let json_bytes = serde_json::to_vec(&serde_json::Value::Object(map)).unwrap();
        let header_len = json_bytes.len() as u64;
        let mut buf = Vec::with_capacity(8 + json_bytes.len());
        buf.extend_from_slice(&header_len.to_le_bytes());
        buf.extend_from_slice(&json_bytes);
        Bytes::from(buf)
    }
}
