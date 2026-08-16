//! Structured reject reasons for per-ref fetch outcomes.
//!
//! Kept separate from push rejection policy because fetch and push protocol
//! surfaces evolve on different cadences.

use std::fmt;

/// Structured reject reason for a per-ref fetch failure.
///
/// Each variant maps to a stable short protocol string via
/// [`FetchRejectReason::protocol_tag`] so git clients and scripts can
/// parse fetch outcomes reliably, and to a human-readable detail via
/// its [`fmt::Display`] impl. Modeled after the smart-HTTP
/// upload-pack ACK surface and the server-side policy knobs it
/// enforces (`uploadpack.allow*SHA1InWant`, `uploadpack.maxEgressBytes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchRejectReason {
    /// The requested SHA is not reachable from any advertised ref.
    /// Covers the `uploadpack.allowReachableSHA1InWant = false` case
    /// and the stricter `allowAnySHA1InWant = false` + non-tip case.
    NotReachable {
        /// Hex OID the client asked for in a `want` line.
        sha: String,
    },
    /// The requested SHA exists on the remote but is not a ref tip.
    /// Emitted when `uploadpack.allowTipSHA1InWant` is the only
    /// allowance in effect and the requested SHA is reachable only
    /// via history, not directly as a branch or tag target.
    NotAtTip {
        /// Hex OID the client asked for in a `want` line.
        sha: String,
    },
    /// Server policy otherwise prohibits fetching this SHA. Catch-all
    /// for rejections that don't cleanly fall under
    /// [`Self::NotReachable`] or [`Self::NotAtTip`] — e.g. a
    /// ref-hide rule from `transfer.hideRefs` denying an
    /// otherwise-reachable object.
    NotAllowed {
        /// Hex OID the client asked for in a `want` line.
        sha: String,
        /// Free-form explanation surfaced in the `Display` output so
        /// operators can tell which policy rule fired.
        reason: String,
    },
    /// Running fetch transfer exceeded `uploadpack.maxEgressBytes`.
    /// Applies to every ref still pending in the batch because the
    /// egress budget is per-fetch, not per-ref.
    EgressTooLarge {
        /// Bytes downloaded at the moment the budget was breached.
        size_bytes: u64,
        /// Configured limit in bytes (`0` means disabled, so this
        /// variant is never emitted for that case).
        limit_bytes: u64,
    },
    /// Git rejected the complete fetched object database after resolving
    /// cross-pack dependencies. Newly installed packs were rolled back before
    /// this variant surfaced.
    MalformedObject {
        /// Pack identity — canonical name of the pack that carried
        /// the malformed object. Preserved so operators can cross-
        /// reference the server-side pack list when triaging.
        pack_id: String,
        /// Hex object id of the failing object.
        oid: String,
        /// Validation scope reported by the pack installer.
        kind: String,
        /// Parser detail — the `gix_object` error that fired.
        detail: String,
    },
    /// Everything else — kept as a last resort so we always have a
    /// variant for uncategorized errors during fetch failures.
    Internal(String),
}

impl FetchRejectReason {
    /// Stable short string suitable for the remote-helper
    /// `error {ref} {tag}` response line. Parseable by scripts and
    /// clients that key off the tag.
    #[must_use]
    pub fn protocol_tag(&self) -> &'static str {
        match self {
            Self::NotReachable { .. } => "not-reachable",
            Self::NotAtTip { .. } => "not-at-tip",
            Self::NotAllowed { .. } => "not-allowed",
            Self::EgressTooLarge { .. } => "egress-too-large",
            Self::MalformedObject { .. } => "malformed-object",
            Self::Internal(_) => "internal",
        }
    }
}

impl fmt::Display for FetchRejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotReachable { sha } => {
                write!(
                    f,
                    "requested sha {sha} is not reachable from any advertised ref"
                )
            }
            Self::NotAtTip { sha } => {
                write!(f, "requested sha {sha} is not a ref tip")
            }
            Self::NotAllowed { sha, reason } => {
                write!(f, "requested sha {sha} not allowed: {reason}")
            }
            Self::EgressTooLarge {
                size_bytes,
                limit_bytes,
            } => write!(
                f,
                "fetch size {size_bytes} bytes exceeds {limit_bytes} byte limit"
            ),
            Self::MalformedObject {
                pack_id,
                oid,
                kind,
                detail,
            } => write!(f, "malformed {kind} {oid} in pack {pack_id}: {detail}"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_reject_protocol_tags_are_stable() {
        assert_eq!(
            FetchRejectReason::NotReachable {
                sha: "deadbeef".into()
            }
            .protocol_tag(),
            "not-reachable"
        );
        assert_eq!(
            FetchRejectReason::NotAtTip {
                sha: "deadbeef".into()
            }
            .protocol_tag(),
            "not-at-tip"
        );
        assert_eq!(
            FetchRejectReason::NotAllowed {
                sha: "deadbeef".into(),
                reason: "hidden ref".into(),
            }
            .protocol_tag(),
            "not-allowed"
        );
        assert_eq!(
            FetchRejectReason::EgressTooLarge {
                size_bytes: 1_000,
                limit_bytes: 500,
            }
            .protocol_tag(),
            "egress-too-large"
        );
        assert_eq!(
            FetchRejectReason::MalformedObject {
                pack_id: "abc".into(),
                oid: "deadbeef".into(),
                kind: "tree".into(),
                detail: "bad entry".into(),
            }
            .protocol_tag(),
            "malformed-object"
        );
        assert_eq!(
            FetchRejectReason::Internal("oops".into()).protocol_tag(),
            "internal"
        );
    }

    #[test]
    fn fetch_reject_display_formats_as_expected() {
        assert_eq!(
            FetchRejectReason::NotReachable {
                sha: "abc123".into()
            }
            .to_string(),
            "requested sha abc123 is not reachable from any advertised ref"
        );
        assert_eq!(
            FetchRejectReason::NotAtTip {
                sha: "abc123".into()
            }
            .to_string(),
            "requested sha abc123 is not a ref tip"
        );
        assert_eq!(
            FetchRejectReason::NotAllowed {
                sha: "abc123".into(),
                reason: "hidden ref".into(),
            }
            .to_string(),
            "requested sha abc123 not allowed: hidden ref"
        );
        assert_eq!(
            FetchRejectReason::EgressTooLarge {
                size_bytes: 2_048,
                limit_bytes: 1_024,
            }
            .to_string(),
            "fetch size 2048 bytes exceeds 1024 byte limit"
        );
        assert_eq!(
            FetchRejectReason::MalformedObject {
                pack_id: "pack-abc".into(),
                oid: "deadbeef".into(),
                kind: "tree".into(),
                detail: "bad entry order".into(),
            }
            .to_string(),
            "malformed tree deadbeef in pack pack-abc: bad entry order"
        );
        assert_eq!(
            FetchRejectReason::Internal("kaboom".into()).to_string(),
            "internal error: kaboom"
        );
    }
}
