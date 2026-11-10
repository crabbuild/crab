use std::{
    io::{Cursor, Read, Seek, Write},
    path::Path,
    process::Command,
    sync::Arc,
};

use super::*;
use crate::repository::{BrowseSession, FileSource};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn safetensors_bytes(name: &str, size: u64) -> Vec<u8> {
    let header = serde_json::to_vec(
        &json!({name: {"dtype": "U8", "shape": [size], "data_offsets": [0, size]}}),
    )
    .unwrap();
    let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
    bytes.extend(header);
    bytes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn safetensors_preview_reads_multi_gib_metadata_without_materializing_payload() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "--initial-branch=main"]);
    let header = safetensors_bytes("weights", 3 * 1024 * 1024 * 1024);
    let path = root.join("weights.safetensors");
    std::fs::write(&path, &header).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(header.len() as u64 + 3 * 1024 * 1024 * 1024)
        .unwrap();
    let repo = BrowseSession::open_local(root, &crate::repository::ReadScope::default())
        .await
        .unwrap();
    let source = FileSource::capture(&repo, "weights.safetensors", None, repo.read_scope())
        .await
        .unwrap();
    let reader = PreviewReader::new(Arc::new(source));
    let preview = tokio::task::spawn_blocking(move || safetensors(reader))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preview["tensors"][0]["nbytes"], 3 * 1024 * 1024 * 1024_u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn safetensors_preview_honors_committed_revision_after_working_file_changes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "--initial-branch=main"]);
    let path = root.join("weights.safetensors");
    let mut committed = safetensors_bytes("committed", 1);
    committed.push(1);
    std::fs::write(&path, committed).unwrap();
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "initial",
        ],
    );
    let mut working = safetensors_bytes("working", 2);
    working.extend([1, 2]);
    std::fs::write(&path, working).unwrap();
    let repo = BrowseSession::open_local(root, &crate::repository::ReadScope::default())
        .await
        .unwrap();
    for (rev, expected) in [(Some("HEAD"), "committed"), (None, "working")] {
        let source = FileSource::capture(&repo, "weights.safetensors", rev, repo.read_scope())
            .await
            .unwrap();
        let reader = PreviewReader::new(Arc::new(source));
        let preview = tokio::task::spawn_blocking(move || safetensors(reader))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(preview["tensors"][0]["name"], expected);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parquet_preview_reads_schema_and_requested_sample_through_captured_source() {
    use arrow::{
        array::Int32Array,
        datatypes::{DataType, Field, Schema},
        record_batch::RecordBatch,
    };
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "--initial-branch=main"]);
    let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int32, false)]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Int32Array::from(vec![1, 2, 3]))],
    )
    .unwrap();
    let file = std::fs::File::create(root.join("data.parquet")).unwrap();
    let mut writer = parquet::arrow::ArrowWriter::try_new(file, schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    let repo = BrowseSession::open_local(root, &crate::repository::ReadScope::default())
        .await
        .unwrap();
    let source = FileSource::capture(&repo, "data.parquet", None, repo.read_scope())
        .await
        .unwrap();
    let reader = PreviewReader::new(Arc::new(source));
    let preview = tokio::task::spawn_blocking(move || parquet(reader, 2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (preview["num_rows"].clone(), preview["preview_rows"].clone()),
        (json!(3), json!([{"n":1}, {"n":2}]))
    );
}

struct ReadLimit<R> {
    inner: R,
    remaining: usize,
}

impl<R: Read> Read for ReadLimit<R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Err(std::io::Error::other(
                "array payload read exceeded metadata budget",
            ));
        }
        let limit = bytes.len().min(self.remaining);
        let length = self.inner.read(&mut bytes[..limit])?;
        self.remaining -= length;
        Ok(length)
    }
}

impl<R: Seek> Seek for ReadLimit<R> {
    fn seek(&mut self, offset: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(offset)
    }
}

#[test]
fn npz_preview_reads_array_headers_without_inflating_array_payloads() {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(
            "array.npy",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
    let mut header = "{'descr': '|u1', 'fortran_order': False, 'shape': (4194304,), }".to_owned();
    while (10 + header.len() + 1) % 64 != 0 {
        header.push(' ');
    }
    header.push('\n');
    writer.write_all(b"\x93NUMPY\x01\x00").unwrap();
    writer
        .write_all(&(header.len() as u16).to_le_bytes())
        .unwrap();
    writer.write_all(header.as_bytes()).unwrap();
    writer.write_all(&vec![0; 4 * 1024 * 1024]).unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    let source = ReadLimit {
        inner: Cursor::new(bytes),
        remaining: 256 * 1024,
    };
    let preview = numpy_npz(source).unwrap();
    assert_eq!(preview["members"][0]["shape"], json!([4194304]));
}

#[test]
fn safetensors_preview_rejects_inconsistent_or_overflowing_tensor_metadata() {
    for tensor in [
        json!({"dtype":"U8", "shape":[4], "data_offsets":[0, 3]}),
        json!({"dtype":"F64", "shape":[u64::MAX], "data_offsets":[0, 0]}),
        json!({"dtype":"U8", "shape":[1], "data_offsets":[1, 2]}),
    ] {
        let header = serde_json::to_vec(&json!({"weights": tensor})).unwrap();
        let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
        bytes.extend(header);
        assert!(safetensors(Cursor::new(bytes)).is_err());
    }
}
