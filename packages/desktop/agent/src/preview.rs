//! Format-specific previewers.
//!
//! Each previewer reads a captured, bounded file source and returns a
//! small JSON payload the renderer can
//! display directly. Parsing happens here rather than in the renderer
//! because:
//!
//!   1. Large files (Parquet, safetensors, NumPy archives) have footer
//!      or header sections we can read cheaply without slurping the
//!      whole blob. Doing that in Rust keeps peak memory bounded.
//!   2. The renderer's JS parsers either don't exist (safetensors) or
//!      happily try to `ArrayBuffer` the whole file (Parquet JS libs),
//!      which blows up on 20 GB datasets.
//!   3. Keeping parser crates on the agent side means the renderer
//!      bundle stays small — Monaco already pushes it to ~6 MB.
//!
//! Every function here returns `Result<Value>` with the agent's
//! error type so dispatch in `handlers.rs` stays symmetric.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{Value, json};

use crate::{AgentError, Result};

fn parse_error(
    context: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> AgentError {
    AgentError::Preview {
        context: context.into(),
        source: Box::new(source),
    }
}

pub(crate) mod query;
mod reader;
pub(crate) use reader::PreviewReader;

/// Default row cap for a Parquet preview when the caller doesn't
/// specify one. 200 rows is a balanced default: one row-group read,
/// under 1 MB of JSON for most schemas.
pub const DEFAULT_PARQUET_PREVIEW_ROWS: usize = 200;

/// Parquet schema + row-group metadata + first `max_rows` rows as JSON.
pub fn parquet(
    file: impl parquet::file::reader::ChunkReader + 'static,
    max_rows: usize,
) -> Result<Value> {
    use arrow::json::{WriterBuilder, writer::JsonArray};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    // Upper bound: refuse anything silly so a "max_rows: 1_000_000"
    // request can't blow the agent's heap.
    let max_rows = max_rows.clamp(1, 5000);

    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| parse_error("parquet", e))?;

    // Capture metadata into owned values up front. The builder is
    // consumed by `.build()` below, but we want schema info and
    // per-column stats in the response — so we extract them now
    // while the builder is still alive.
    let meta = builder.metadata().clone();
    let file_md = meta.file_metadata();
    let num_rows = file_md.num_rows();
    let num_row_groups = meta.num_row_groups();
    let num_columns = file_md.schema().get_fields().len();
    let created_by = file_md.created_by().map(std::string::ToString::to_string);

    // Column summary from the Arrow schema — Parquet's own logical
    // types are noisy; the Arrow-projected schema is what a user
    // actually cares about.
    let arrow_schema = builder.schema().clone();
    let columns: Vec<Value> = arrow_schema
        .fields()
        .iter()
        .map(|f| {
            json!({
                "name": f.name(),
                "type": f.data_type().to_string(),
                "nullable": f.is_nullable(),
            })
        })
        .collect();

    // Per-column basic stats from the first row group. Bounded and
    // fast; a full scan would be unreasonable on multi-GB files.
    let mut stats: Vec<Value> = Vec::with_capacity(num_columns);
    if num_row_groups > 0 {
        let rg = meta.row_group(0);
        for col in rg.columns() {
            let s = col.statistics();
            stats.push(json!({
                "column": col.column_descr().name(),
                "null_count": s.and_then(|s| s.null_count_opt()),
                "distinct_count": s.and_then(|s| s.distinct_count_opt()),
                "compression": format!("{:?}", col.compression()),
                "compressed_size": col.compressed_size(),
                "uncompressed_size": col.uncompressed_size(),
            }));
        }
    }

    // Read first row group, capped to N rows. Consumes the builder.
    let reader = builder
        .with_batch_size(max_rows)
        .with_limit(max_rows)
        .build()
        .map_err(|e| parse_error("parquet reader", e))?;

    let mut rows_json: Vec<u8> = Vec::new();
    {
        let mut writer = WriterBuilder::new()
            .with_explicit_nulls(true)
            .build::<_, JsonArray>(&mut rows_json);
        for batch in reader {
            let batch = batch.map_err(|e| parse_error("parquet batch", e))?;
            writer
                .write(&batch)
                .map_err(|e| parse_error("arrow→json", e))?;
        }
        writer.finish().map_err(|e| parse_error("arrow→json", e))?;
    }
    let rows: Value = serde_json::from_slice(&rows_json)?;

    Ok(json!({
        "kind": "parquet",
        "num_rows": num_rows,
        "num_row_groups": num_row_groups,
        "num_columns": num_columns,
        "created_by": created_by,
        "columns": columns,
        "row_group_stats": stats,
        "preview_rows": rows,
        "preview_row_count": rows.as_array().map_or(0, Vec::len),
    }))
}

/// Safetensors header: the file starts with an 8-byte little-endian
/// `u64` giving the JSON header byte length, followed by the header
/// JSON, followed by raw tensor bytes. Parse the header without
/// materializing tensors; the range reader may prefetch nearby bytes.
pub fn safetensors(mut file: impl Read + Seek) -> Result<Value> {
    let file_size = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;
    let mut len_buf = [0u8; 8];
    file.read_exact(&mut len_buf)?;
    let header_len = usize::try_from(u64::from_le_bytes(len_buf)).map_err(std::io::Error::other)?;
    if header_len > 100_000_000 || header_len as u64 > file_size.saturating_sub(8) {
        return Err(AgentError::Other(
            "safetensors header exceeds file size or 100 MB limit".into(),
        ));
    }
    let mut bytes = vec![0; header_len];
    file.read_exact(&mut bytes)?;
    let header: safetensors::tensor::Metadata = serde_json::from_slice(&bytes)?;
    let mut entries: Vec<_> = header.tensors().into_iter().collect();
    entries.sort_by_key(|(_, info)| info.data_offsets);
    let mut end = 0usize;
    let mut total_params = 0u128;
    let mut dtype_hist = std::collections::BTreeMap::<String, u64>::new();
    let mut tensors = Vec::with_capacity(entries.len());
    for (name, tensor) in entries {
        let (start, next) = tensor.data_offsets;
        let count = tensor
            .shape
            .iter()
            .copied()
            .try_fold(1usize, usize::checked_mul)
            .ok_or_else(|| AgentError::Other("safetensors tensor shape overflows".into()))?;
        let size = count
            .checked_mul(tensor.dtype.size())
            .ok_or_else(|| AgentError::Other("safetensors tensor size overflows".into()))?;
        // Metadata::deserialize does not call the dependency's private validate.
        // Apply its contiguous-offset/shape contract without reading tensor bodies.
        if start != end || next.checked_sub(start) != Some(size) {
            return Err(AgentError::Other(
                "safetensors tensor offsets disagree with its shape or dtype".into(),
            ));
        }
        end = next;
        total_params += count as u128;
        let dtype = format!("{:?}", tensor.dtype);
        *dtype_hist.entry(dtype.clone()).or_default() += 1;
        tensors.push(json!({"name": name, "dtype": dtype, "shape": tensor.shape, "nbytes": size}));
    }
    let data_size = file_size - 8 - header_len as u64;
    if end as u64 != data_size {
        return Err(AgentError::Other(
            "safetensors tensor offsets do not cover the file payload".into(),
        ));
    }
    tensors.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    Ok(json!({
        "kind": "safetensors",
        "header_bytes": header_len,
        "tensor_count": tensors.len(),
        "total_params": total_params.to_string(),
        "total_bytes": data_size.to_string(),
        "dtype_histogram": dtype_hist,
        "metadata": header.metadata(),
        "tensors": tensors,
    }))
}

/// NumPy `.npy` header parse.
pub fn numpy_npy(file: impl Read) -> Result<Value> {
    let npy = npyz::NpyFile::new(file).map_err(|e| parse_error("npy", e))?;
    let header = npy.header();
    let shape: Vec<u64> = header.shape().to_vec();
    let dtype = format!("{:?}", header.dtype());
    let order = format!("{:?}", header.order());
    let total_len = shape
        .iter()
        .try_fold(1u128, |total, &dimension| {
            total.checked_mul(u128::from(dimension))
        })
        .ok_or_else(|| AgentError::Other("NumPy shape overflows its element count".into()))?;

    Ok(json!({
        "kind": "npy",
        "shape": shape,
        "dtype": dtype,
        "order": order,
        "num_elements": total_len.to_string(),
    }))
}

/// NumPy `.npz` archive: list the contained `.npy` members with
/// shapes/dtypes. Implemented by treating the `.npz` as a zip archive
/// and parsing each member's npy header.
pub fn numpy_npz(file: impl Read + Seek) -> Result<Value> {
    let mut archive = zip::ZipArchive::new(file).map_err(|e| parse_error("npz zip", e))?;
    let mut members: Vec<Value> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| parse_error(format!("npz member {i}"), e))?;
        let name = entry.name().to_owned();
        if !name.ends_with(".npy") {
            continue;
        }
        let size = entry.size();
        // Array payloads can be multi-GiB; only the member header is needed.
        match npyz::NpyFile::new(&mut entry) {
            Ok(npy) => {
                let header = npy.header();
                members.push(json!({
                    "name": name.trim_end_matches(".npy"),
                    "shape": header.shape().to_vec(),
                    "dtype": format!("{:?}", header.dtype()),
                    "nbytes": size,
                }));
            }
            Err(e) => {
                members.push(json!({
                    "name": name.trim_end_matches(".npy"),
                    "error": format!("{e}"),
                }));
            }
        }
    }
    Ok(json!({
        "kind": "npz",
        "members": members,
    }))
}

/// Zip archive member listing.
pub fn zip_archive(file: impl Read + Seek) -> Result<Value> {
    let mut archive = zip::ZipArchive::new(file).map_err(|e| parse_error("zip", e))?;
    let mut members: Vec<Value> = Vec::with_capacity(archive.len());
    let mut total_compressed: u128 = 0;
    let mut total_uncompressed: u128 = 0;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| parse_error(format!("zip member {i}"), e))?;
        total_compressed += u128::from(entry.compressed_size());
        total_uncompressed += u128::from(entry.size());
        members.push(json!({
            "name": entry.name(),
            "is_dir": entry.is_dir(),
            "size": entry.size(),
            "compressed": entry.compressed_size(),
            "crc32": entry.crc32(),
        }));
    }
    Ok(json!({
        "kind": "zip",
        "member_count": members.len(),
        "total_compressed": total_compressed.to_string(),
        "total_uncompressed": total_uncompressed.to_string(),
        "members": members,
    }))
}

/// Jupyter notebook metadata: kernel info, cell counts, output presence.
///
/// Notebooks are JSON files with a well-known schema (nbformat 4+).
/// We parse the top-level structure and extract summary metadata
/// without loading cell contents into memory beyond what serde needs.
pub fn notebook_meta(mut file: impl Read) -> Result<Value> {
    let mut contents = String::new();
    file.read_to_string(&mut contents).map_err(AgentError::Io)?;

    let nb: Value =
        serde_json::from_str(&contents).map_err(|e| parse_error("notebook JSON parse", e))?;

    let obj = nb
        .as_object()
        .ok_or_else(|| AgentError::Other("notebook: top-level value is not an object".into()))?;

    // nbformat version
    let nbformat = obj.get("nbformat").and_then(Value::as_u64);

    // Kernel metadata — try multiple paths used across nbformat versions.
    let kernelspec = obj
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|m| m.get("kernelspec"))
        .and_then(Value::as_object);

    let kernel_name = kernelspec
        .and_then(|ks| {
            ks.get("display_name")
                .and_then(Value::as_str)
                .or_else(|| ks.get("name").and_then(Value::as_str))
        })
        .map(String::from);

    let kernel_language = kernelspec
        .and_then(|ks| ks.get("language").and_then(Value::as_str))
        .or_else(|| {
            obj.get("metadata")
                .and_then(Value::as_object)
                .and_then(|m| m.get("language_info"))
                .and_then(Value::as_object)
                .and_then(|li| li.get("name").and_then(Value::as_str))
        })
        .map(String::from);

    // Cell statistics
    let cells = obj.get("cells").and_then(Value::as_array);
    let cell_count = cells.map(Vec::len).unwrap_or(0);

    let (code_cell_count, markdown_cell_count, has_outputs) = cells
        .map(|cells| {
            let mut code = 0usize;
            let mut markdown = 0usize;
            let mut any_output = false;
            for cell in cells {
                let cell_type = cell.get("cell_type").and_then(Value::as_str).unwrap_or("");
                match cell_type {
                    "code" => {
                        code += 1;
                        if !any_output {
                            any_output = cell
                                .get("outputs")
                                .and_then(Value::as_array)
                                .is_some_and(|o| !o.is_empty());
                        }
                    }
                    "markdown" => markdown += 1,
                    _ => {}
                }
            }
            (code, markdown, any_output)
        })
        .unwrap_or((0, 0, false));

    Ok(json!({
        "kind": "notebook",
        "kernel_name": kernel_name,
        "kernel_language": kernel_language,
        "nbformat": nbformat,
        "cell_count": cell_count,
        "code_cell_count": code_cell_count,
        "markdown_cell_count": markdown_cell_count,
        "has_outputs": has_outputs,
    }))
}

/// PyTorch checkpoint preview.
///
/// PyTorch checkpoints are typically ZIP archives containing pickled
/// state dicts (e.g. `archive/data.pkl`, `archive/data/0`, etc.).
/// We parse the ZIP structure to extract metadata without loading
/// tensors or unpickling Python objects.
///
/// For older pickle-only `.pt` files that aren't valid ZIPs, we fall
/// back to reporting basic file info.
pub fn torch_checkpoint(mut file: impl Read + Seek) -> Result<Value> {
    let file_size = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;

    // Try to open as a ZIP archive. PyTorch's `torch.save` uses
    // zipfile by default since PyTorch 1.6+.
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(zip::result::ZipError::Io(error)) => return Err(error.into()),
        Err(_) => {
            // Not a valid ZIP — likely an older pickle-only format.
            return Ok(json!({
                "kind": "torch_checkpoint",
                "format": "pickle",
                "file_size_bytes": file_size,
                "zip_entries": [],
                "has_optimizer": false,
                "num_tensors": 0,
            }));
        }
    };

    let mut zip_entries: Vec<String> = Vec::with_capacity(archive.len());
    let mut num_tensors: u64 = 0;
    let mut has_optimizer = false;

    for i in 0..archive.len() {
        let entry = match archive.by_index_raw(i) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.name().to_owned();

        // Each tensor is stored as a separate file under `data/` or
        // `archive/data/` with a numeric name (0, 1, 2, …).
        if name.contains("data/") && !name.ends_with('/') {
            // Exclude the pkl files from the tensor count.
            if !name.ends_with(".pkl") {
                num_tensors += 1;
            }
        }

        // Heuristic: optimizer state is present if any entry name
        // contains "optimizer".
        if !has_optimizer {
            let lower = name.to_lowercase();
            if lower.contains("optimizer") {
                has_optimizer = true;
            }
        }

        zip_entries.push(name);
    }

    Ok(json!({
        "kind": "torch_checkpoint",
        "format": "pytorch",
        "file_size_bytes": file_size,
        "zip_entries": zip_entries,
        "has_optimizer": has_optimizer,
        "num_tensors": num_tensors,
    }))
}

/// Peek at a file's first N bytes for magic-number sniffing when the
/// extension is ambiguous. Currently unused; left here because the
/// pattern shows up repeatedly and future formats (HDF5, etc.) will
/// want it.
#[allow(dead_code)]
pub fn peek(path: &Path, n: usize) -> Result<Vec<u8>> {
    let mut file = File::open(path).map_err(AgentError::Io)?;
    let mut buf = vec![0u8; n];
    let read = file.read(&mut buf).map_err(AgentError::Io)?;
    buf.truncate(read);
    file.seek(SeekFrom::Start(0)).ok();
    Ok(buf)
}

#[cfg(test)]
mod tests;
