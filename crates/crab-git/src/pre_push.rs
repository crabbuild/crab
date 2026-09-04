//! Bounded decoding of Git's complete pre-push hook input.

use std::collections::HashSet;
use std::io::{self, Read};

/// One frozen update reported by Git to a pre-push hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrePushUpdate {
    /// Exact object being sent, or `None` for an explicit deletion.
    pub local_oid: Option<String>,
    /// Fully qualified destination name, which need not match the local name.
    pub remote_ref: String,
    /// Advertised value at the pushed-to remote, or `None` for a creation.
    /// This is not an expected-old value for a separate mirror remote.
    pub remote_oid: Option<String>,
}

/// Read and validate the whole Git hook batch before returning any updates.
///
/// Reads at most `max_bytes + 1` bytes. Rejects oversized, unterminated, malformed,
/// duplicate-destination, non-UTF-8, and mixed-object-format input. SHA-1 and
/// SHA-256 records are accepted; this does not establish transport support.
pub fn read_pre_push(input: impl Read, max_bytes: u64) -> io::Result<Vec<PrePushUpdate>> {
    let mut text = String::new();
    input
        .take(max_bytes.saturating_add(1))
        .read_to_string(&mut text)?;
    if text.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("pre-push input exceeds {max_bytes} bytes"),
        ));
    }

    let mut updates = Vec::new();
    let mut destinations = HashSet::new();
    let mut oid_width = None;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let invalid = |reason: &str| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid pre-push input on line {}: {reason}", index + 1),
            )
        };
        if !line.ends_with('\n') {
            return Err(invalid("unterminated ref update"));
        }
        let mut fields = line.split_ascii_whitespace();
        let (Some(local_ref), Some(local_oid), Some(remote_ref), Some(remote_oid), None) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err(invalid("expected four fields"));
        };
        if !is_object_id(local_oid)
            || !is_object_id(remote_oid)
            || local_oid.len() != remote_oid.len()
        {
            return Err(invalid("expected same-format full object IDs"));
        }
        if oid_width.is_some_and(|width| width != local_oid.len()) {
            return Err(invalid("mixed Git object formats"));
        }
        oid_width = Some(local_oid.len());
        let deleted = is_zero(local_oid);
        if deleted != (local_ref == "(delete)") {
            return Err(invalid("deletion marker and local object ID disagree"));
        }
        if local_ref.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(invalid("control byte in local revision"));
        }
        if !remote_ref.starts_with("refs/") {
            return Err(invalid("destination must be a fully qualified ref"));
        }
        crate::refname::validate_push_refname(remote_ref)
            .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?;
        if !destinations.insert(remote_ref) {
            return Err(invalid("duplicate destination ref"));
        }

        // Git may supply HEAD~, a raw OID, or a ref that moves while the hook
        // runs. Only the supplied OID identifies what this push actually sends.
        updates.push(PrePushUpdate {
            local_oid: (!deleted).then(|| local_oid.to_ascii_lowercase()),
            remote_ref: remote_ref.to_owned(),
            remote_oid: (!is_zero(remote_oid)).then(|| remote_oid.to_ascii_lowercase()),
        });
    }
    Ok(updates)
}

fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_zero(value: &str) -> bool {
    value.bytes().all(|byte| byte == b'0')
}

#[cfg(test)]
mod tests;
