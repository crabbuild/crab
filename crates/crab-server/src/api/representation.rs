use super::ApiError;
use crate::repository::projection;
use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use crab_remote_git::{
    Blob, BudgetDimension, Commit, CommitSummary, ContentClassification, OperationContext,
    PageCursor, RemoteGitSnapshot, TreeEntry,
};
use serde_json::{Value, json};
use uuid::Uuid;
pub(super) const MAX_INLINE_TEXT_BYTES: usize = 1024 * 1024;
#[cfg(test)]
use crab_remote_git::GitPath;

#[derive(serde::Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum FilePreviewState {
    Inline { text: String },
    OmittedSize,
    Binary,
    UnsupportedEncoding,
}

#[derive(serde::Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(super) struct FilePreview {
    oid: String,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    stored_size: Option<u64>,
    mode: String,
    classification: String,
    commit: String,
    generation: u64,
    preview: FilePreviewState,
}

impl FilePreview {
    pub(super) fn maximum_encoded_bytes(&self) -> usize {
        let text = match &self.preview {
            FilePreviewState::Inline { text } => text.len(),
            FilePreviewState::OmittedSize
            | FilePreviewState::Binary
            | FilePreviewState::UnsupportedEncoding => 0,
        };
        let strings = [&self.oid, &self.mode, &self.classification, &self.commit];
        let bytes = strings
            .into_iter()
            .fold(text, |bytes, value| bytes.saturating_add(value.len()));
        // JSON escapes expand one input byte by at most six bytes. The fixed
        // field names, enum tags, punctuation and numeric fields fit within 256 bytes.
        bytes.saturating_mul(6).saturating_add(256)
    }
}

#[cfg(test)]
pub(super) async fn file_preview(
    snapshot: &RemoteGitSnapshot,
    path: &GitPath,
    operation: &OperationContext,
) -> crab_remote_git::Result<FilePreview> {
    let metadata = snapshot.blob_metadata(path, operation).await?;
    file_preview_metadata(snapshot, metadata, operation).await
}

pub(super) async fn file_preview_metadata(
    snapshot: &RemoteGitSnapshot,
    metadata: crab_remote_git::BlobMetadata,
    operation: &OperationContext,
) -> crab_remote_git::Result<FilePreview> {
    let mut value = FilePreview {
        oid: metadata.oid.to_string(),
        size: metadata.git_size,
        stored_size: metadata.logical_size.filter(|_| {
            matches!(
                metadata.classification,
                ContentClassification::LfsPointer | ContentClassification::CrabPointer
            )
        }),
        mode: format!("{:06o}", metadata.mode.raw()),
        classification: format!("{:?}", metadata.classification),
        commit: snapshot.commit_oid().to_string(),
        generation: snapshot.generation(),
        preview: FilePreviewState::OmittedSize,
    };
    let metadata_bytes = std::mem::size_of::<FilePreview>()
        + value.oid.len()
        + value.mode.len()
        + value.classification.len()
        + value.commit.len();
    operation
        .charge(BudgetDimension::ResponseBytes, metadata_bytes as u64)
        .await?;
    if metadata.git_size > MAX_INLINE_TEXT_BYTES as u64 {
        return Ok(value);
    }
    // Metadata proved the path's visibility and inspected pointer-sized bytes.
    // Read only the bounded Git representation, charging returned fields rather
    // than an omitted body; read_blob's full-body response fee does not apply.
    let object = operation.read_object(metadata.oid).await?;
    value.preview = if object.data.contains(&0) {
        FilePreviewState::Binary
    } else if let Ok(text) = std::str::from_utf8(&object.data) {
        operation
            .charge(BudgetDimension::ResponseBytes, text.len() as u64)
            .await?;
        FilePreviewState::Inline {
            text: text.to_owned(),
        }
    } else {
        FilePreviewState::UnsupportedEncoding
    };
    Ok(value)
}

pub(super) fn blob_response(blob: Blob, range: Option<&HeaderValue>) -> Response {
    let total = blob.bytes.len();
    let requested = range.map(|value| {
        value
            .to_str()
            .map_err(|_| ())
            .and_then(|value| parse_byte_range(value, total))
    });
    let (status, body, content_range) = match requested {
        None => (StatusCode::OK, blob.bytes, None),
        Some(Ok((start, end))) => {
            let body = blob.bytes.slice(start..end);
            (
                StatusCode::PARTIAL_CONTENT,
                body,
                Some(format!("bytes {start}-{}/{total}", end - 1)),
            )
        }
        Some(Err(())) => {
            let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{total}")) {
                response.headers_mut().insert(header::CONTENT_RANGE, value);
            }
            response
                .headers_mut()
                .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            return response;
        }
    };
    let oid = blob.metadata.oid.to_string();
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment"),
    );
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(value) = HeaderValue::from_str(&oid) {
        response.headers_mut().insert("x-crab-blob-oid", value);
    }
    if let Some(content_range) = content_range
        && let Ok(value) = HeaderValue::from_str(&content_range)
    {
        response.headers_mut().insert(header::CONTENT_RANGE, value);
    }
    response
}

pub(super) fn parse_byte_range(value: &str, total: usize) -> Result<(usize, usize), ()> {
    let range = value.strip_prefix("bytes=").ok_or(())?;
    if total == 0 || range.contains(',') {
        return Err(());
    }
    let (start, end) = range.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = end.parse::<usize>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        return Ok((total.saturating_sub(suffix), total));
    }
    let start = start.parse::<usize>().map_err(|_| ())?;
    if start >= total {
        return Err(());
    }
    let end = if end.is_empty() {
        total
    } else {
        end.parse::<usize>()
            .map_err(|_| ())?
            .saturating_add(1)
            .min(total)
    };
    if start >= end {
        return Err(());
    }
    Ok((start, end))
}

pub(super) fn content_json(blob: &Blob) -> Value {
    let decoded = if blob.bytes.contains(&0) {
        None
    } else {
        std::str::from_utf8(&blob.bytes).ok()
    };
    let text = decoded.filter(|_| blob.bytes.len() <= MAX_INLINE_TEXT_BYTES);
    json!({"oid":blob.metadata.oid.to_string(),"size":blob.bytes.len(),"mode":format!("{:06o}",blob.metadata.mode.raw()),"classification":format!("{:?}",blob.metadata.classification),"text":text,"text_truncated":decoded.is_some() && text.is_none()})
}

pub(super) fn image_content_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub(super) fn asset_response(blob: Blob) -> Response {
    match image_content_type(&blob.bytes) {
        Some(content_type) => (
            [
                ("content-type", content_type.to_owned()),
                ("x-crab-blob-oid", blob.metadata.oid.to_string()),
            ],
            blob.bytes,
        )
            .into_response(),
        None => (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({"error":{"code":"unsupported_asset","message":"Only PNG, JPEG, GIF and WebP blobs can be displayed inline"}})),
        )
            .into_response(),
    }
}

pub(super) fn entry_json(entry: &TreeEntry) -> Value {
    json!({"path":display_path(entry.path.as_bytes()),"path_hex":encode_hex(entry.path.as_bytes()),"oid":entry.oid.to_string(),"mode":format!("{:06o}",entry.mode.raw()),"kind":format!("{:?}",entry.kind)})
}

pub(super) fn commit_json(commit: &Commit) -> Value {
    json!({"oid":commit.oid.to_string(),"tree":commit.tree.to_string(),"parents":commit.parents.iter().map(ToString::to_string).collect::<Vec<_>>(),"author":String::from_utf8_lossy(&commit.author.name),"author_seconds":commit.author.seconds,"message":String::from_utf8_lossy(&commit.message),"message_hex":encode_hex(&commit.message)})
}

pub(super) fn commit_summary_json(commit: &CommitSummary) -> Value {
    json!({"oid":commit.oid.to_string(),"author":String::from_utf8_lossy(&commit.author),"author_seconds":commit.author_seconds,"message":String::from_utf8_lossy(&commit.message)})
}

pub(super) fn json_attribution_summary(item: &projection::AttributionItem) -> Value {
    json!({
        "oid": encode_hex(&item.commit_oid),
        "author": String::from_utf8_lossy(&item.author),
        "author_seconds": item.author_seconds,
        "message": String::from_utf8_lossy(&item.message),
    })
}

pub(super) fn path_history_json(entry: &crab_remote_git::PathHistoryEntry) -> Value {
    let mut value = commit_json(&entry.commit);
    value["change_kind"] = json!(format!("{:?}", entry.kind));
    value
}

pub(crate) fn display_path(bytes: &[u8]) -> String {
    bytes
        .split(|byte| *byte == b'/')
        .map(|component| match std::str::from_utf8(component) {
            Ok(text) => text.replace('%', "%25"),
            Err(_) => component
                .iter()
                .map(|byte| format!("%{byte:02X}"))
                .collect(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub(super) fn encode_cursor(key: &[u8; 32], repository: Uuid, cursor: PageCursor) -> String {
    format!(
        "v2.{}.{}.{}",
        repository.simple(),
        encode_hex(cursor.as_bytes()),
        cursor_signature(key, repository, cursor.as_bytes()).to_hex()
    )
}

fn cursor_signature(key: &[u8; 32], repository: Uuid, payload: &[u8]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new_keyed(key);
    hasher.update(b"crab-server.page-cursor.v2\0");
    hasher.update(repository.as_bytes());
    hasher.update(payload);
    hasher.finalize()
}

pub(super) fn decode_cursor(
    key: &[u8; 32],
    repository: Uuid,
    value: &str,
) -> std::result::Result<PageCursor, ApiError> {
    let (payload, signature, repository_bound) = if let Some(value) = value.strip_prefix("v2.") {
        let (scope, value) = value
            .split_once('.')
            .ok_or(ApiError::Input("Invalid cursor"))?;
        if scope != repository.simple().to_string() {
            return Err(ApiError::Input("Cursor belongs to another repository"));
        }
        let (payload, signature) = value
            .split_once('.')
            .ok_or(ApiError::Input("Invalid cursor"))?;
        (payload, signature, true)
    } else {
        // Alpha.1 exposed this signed envelope without UUID binding. Retain its
        // reader for those issued tokens; all new tokens use the scoped format.
        let (payload, signature) = value
            .split_once('.')
            .ok_or(ApiError::Input("Invalid cursor"))?;
        (payload, signature, false)
    };
    let bytes = decode_hex(payload)?;
    let signature = decode_hex(signature)?;
    let expected = if repository_bound {
        cursor_signature(key, repository, &bytes)
    } else {
        blake3::keyed_hash(key, &bytes)
    };
    if expected != *signature.as_slice() {
        return Err(ApiError::Input("Invalid cursor signature"));
    }
    Ok(PageCursor::from_bytes(bytes)?)
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub(super) fn decode_hex(value: &str) -> std::result::Result<Vec<u8>, ApiError> {
    if !value.len().is_multiple_of(2)
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        || value.len() > 128 * 1024
    {
        return Err(ApiError::Input("Invalid hex bytes"));
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16)
                .map_err(|_| ApiError::Input("Invalid hex bytes"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_encoding_bound_covers_escaped_text_and_metadata() {
        for preview in [
            FilePreviewState::OmittedSize,
            FilePreviewState::Binary,
            FilePreviewState::UnsupportedEncoding,
            FilePreviewState::Inline {
                text: "\u{1}\"\n\\é🐚".repeat(20_000),
            },
        ] {
            let value = FilePreview {
                oid: "\"\\\u{1}".repeat(20),
                size: u64::MAX,
                stored_size: Some(u64::MAX),
                mode: "\"\\\u{1}".repeat(20),
                classification: "\"\\\u{1}".repeat(20),
                commit: "\"\\\u{1}".repeat(20),
                generation: u64::MAX,
                preview,
            };
            let bytes = serde_json::to_vec(&value).unwrap();
            assert!(bytes.len() <= value.maximum_encoded_bytes());
        }
    }
}
