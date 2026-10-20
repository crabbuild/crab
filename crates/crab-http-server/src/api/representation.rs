use super::ApiError;
use crate::projection;
use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use crab_remote_git::{Blob, Commit, CommitSummary, PageCursor, TreeEntry};
use serde_json::{Value, json};
pub(super) const MAX_INLINE_TEXT_BYTES: usize = 1024 * 1024;

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

fn parse_byte_range(value: &str, total: usize) -> Result<(usize, usize), ()> {
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

pub(super) fn encode_cursor(key: &[u8; 32], cursor: PageCursor) -> String {
    format!(
        "{}.{}",
        encode_hex(cursor.as_bytes()),
        blake3::keyed_hash(key, cursor.as_bytes()).to_hex()
    )
}

pub(super) fn decode_cursor(
    key: &[u8; 32],
    value: &str,
) -> std::result::Result<PageCursor, ApiError> {
    let (payload, signature) = value
        .split_once('.')
        .ok_or(ApiError::Input("Invalid cursor"))?;
    let bytes = decode_hex(payload)?;
    let signature = decode_hex(signature)?;
    if blake3::keyed_hash(key, &bytes) != *signature.as_slice() {
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
