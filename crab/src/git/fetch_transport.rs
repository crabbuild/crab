//! Compatibility helpers for the released gix transport API.
//!
//! Production protocol-v2 fetches are served by
//! [`crate::git::upload_pack_wire`], where the local remote helper performs
//! the upload-pack protocol role. The client-oriented gix transport scaffold
//! remains behind its released feature for source compatibility, but is
//! deliberately deprecated and fails closed if called.
//!
//! The other helpers in this module are still useful, narrowly scoped
//! gitoxide adapters:
//!
//! - [`StdioTransport`] — the retained, deprecated client transport type.
//! - [`parse_refspec_gix`] — refspec parsing via `gix_refspec::parse`,
//!   gated behind `gix-transport` to keep the legacy hand-rolled
//!   `push <src>:<dst>` parser in `parse_command` serving builds
//!   without the feature.
//! - [`build_ref_advertisement_typed`] — typed-ref → helper-line
//!   formatter that walks `gix_ref::Reference` values from
//!   [`crab_git::ref_resolve::resolve_refs_typed_batch`] instead of
//!   string-concatenating from a [`crate::git::remote_helper::ListOutput`].
//! - [`negotiate_session`] — a small adapter for callers that still need the
//!   released gix negotiation type; production upload-pack negotiation is
//!   owned by the local wire path.
//! - [`remote_origin_url`] — `gix::Repository::find_remote("origin")`
//!   replacement for the legacy `git remote get-url origin` shellout
//!   at the fetch-path write site.
//!
//! The code here is kept feature-gated because the public transport type was
//! released. It is not the owner of the terminal helper session, and the
//! capability advertisement is controlled by the proof-gated
//! [`crate::git::remote_helper::format_capabilities_with_v2`].
//!
//! The remaining adapters are intentionally independent of that session:
//!
//! - **Compatibility transport** — implements the released `Transport` /
//!   `TransportWithoutIO` traits but remains fail-closed.
//! - **5.2 Typed ref advertisement** — ships an alternative
//!   formatter; the remote helper's `list` branch still uses the
//!   string builder until we flip one call site behind the feature.
//! - **5.3 Negotiation scaffold** — constructs the algorithm state
//!   machine without invoking it.
//! - **5.4 Refspec parse** — the gix-backed parser lives here; the
//!   outer `parse_command` still dispatches push lines with the
//!   legacy split when `gix-transport` is off.
//! - **Capability advertisement** — see
//!   [`crate::git::remote_helper::format_capabilities_with_v2`].
//! - **5.6 `remote get-url origin`** — [`remote_origin_url`] is the
//!   call site; the legacy shellout in [`crate::git::remote_helper::fetch_packs`]
//!   is feature-gated to prefer it when the feature is on.
//!
//! ## Boundary tracing
//!
//! Every public function in this module wraps its gitoxide call(s)
//! in the [`crate::gix_boundary!`] span so flamegraphs attribute
//! CPU time to the gitoxide side of the adoption frontier.

#![cfg(feature = "gix-transport")]
#![expect(
    deprecated,
    reason = "the released StdioTransport compatibility surface is intentionally retained here"
)]

use std::any::Any;
use std::borrow::Cow;
use std::io::{BufRead, BufReader, Stdin, Stdout, Write};

use bstr::{BStr, BString, ByteSlice};

use gix_ref::Reference;
use gix_transport::Protocol;
use gix_transport::client::blocking_io::{
    RequestWriter, SetServiceResponse, Transport as BlockingTransport,
};
use gix_transport::client::{Error as TransportError, MessageKind, TransportWithoutIO, WriteMode};

use crate::core::error::{CrabError, Result};

// --- 5.4 Refspec parse via gix_refspec ------------------------------

/// A refspec parsed by [`gix_refspec`].
///
/// Carries the three fields the push command needs (`force`,
/// `src`, `dst`) in the same shape [`crate::git::remote_helper::PushSpec`]
/// already uses, so swapping the hand-rolled parser for this one is
/// mechanical once we flip the feature flag on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPushRefspec {
    /// Whether the client asked for a force update (`+src:dst`).
    pub force: bool,
    /// The source ref-or-pattern on the local side.
    pub src: String,
    /// The destination ref-or-pattern on the remote side.
    pub dst: String,
}

/// Parse a push refspec string via `gix_refspec::parse`.
///
/// Accepts the git-standard form the remote helper protocol sends:
///
/// ```text
/// push <src>:<dst>
/// push +<src>:<dst>           // force
/// push :<dst>                 // delete
/// ```
///
/// The `push ` prefix MUST already be stripped by the caller —
/// `gix_refspec::parse` takes just the refspec body. Returns
/// [`CrabError::Protocol`] when the input is not a recognized
/// push-direction refspec, or when the parser returns an
/// unsupported instruction variant (e.g. `AllMatchingBranches`,
/// `Exclude`) that the legacy hand-rolled parser never produced
/// and that crab's push pipeline doesn't know how to apply.
pub fn parse_refspec_gix(spec: &str) -> Result<ParsedPushRefspec> {
    let _span = crate::gix_boundary!("refspec", "parse").entered();

    // `gix_refspec::parse` wants a `&BStr`. Go through `as_bytes()`
    // so non-UTF-8 inputs surface as a parse error rather than
    // panicking in the conversion.
    let as_bstr: &BStr = spec.as_bytes().as_bstr();

    let parsed = gix_refspec::parse(as_bstr, gix_refspec::parse::Operation::Push)
        .map_err(|e| CrabError::Protocol(format!("refspec parse failed for {spec:?}: {e}")))?;

    match parsed.instruction() {
        gix_refspec::Instruction::Push(push) => match push {
            gix_refspec::instruction::Push::Matching {
                src,
                dst,
                allow_non_fast_forward,
            } => Ok(ParsedPushRefspec {
                force: allow_non_fast_forward,
                src: bstr_to_string(src)?,
                dst: bstr_to_string(dst)?,
            }),
            gix_refspec::instruction::Push::Delete { ref_or_pattern } => {
                // Helper-protocol push delete is serialized as
                // `push :<dst>`, which crab currently parses as
                // `src = ""`, `dst = <ref>`. Stay byte-compatible
                // with that so the rest of the push pipeline keeps
                // working when this parser replaces the hand-rolled
                // split.
                Ok(ParsedPushRefspec {
                    force: false,
                    src: String::new(),
                    dst: bstr_to_string(ref_or_pattern)?,
                })
            }
            gix_refspec::instruction::Push::AllMatchingBranches { .. }
            | gix_refspec::instruction::Push::Exclude { .. } => Err(CrabError::Protocol(format!(
                "unsupported push refspec shape for crab: {spec:?}"
            ))),
        },
        gix_refspec::Instruction::Fetch(_) => Err(CrabError::Protocol(format!(
            "expected push refspec, got fetch: {spec:?}"
        ))),
    }
}

fn bstr_to_string(b: &BStr) -> Result<String> {
    std::str::from_utf8(b)
        .map(ToOwned::to_owned)
        .map_err(|e| CrabError::Protocol(format!("refspec side is not UTF-8: {e}")))
}

// --- 5.2 Typed ref advertisement ------------------------------------

/// Emit the helper-protocol `list` response by iterating typed
/// [`gix_ref::Reference`] values.
///
/// Drop-in replacement for the string-builder path in
/// [`crate::git::remote_helper::format_list_output`]. The byte-level
/// output is intentionally identical so a client can't tell whether
/// the bytes came from the legacy string builder or the typed walker
/// — only the construction path changes. Call sites that want to
/// flip behind `gix-transport` compare the two outputs in tests.
///
/// `refs` is an iterator of `(input_name, Reference)` pairs, matching
/// the shape [`crab_git::ref_resolve::resolve_refs_typed_batch`]
/// returns. `head_symref_target` is the ref name HEAD points at, if
/// any; an unborn symbolic target need not occur among the concrete refs.
/// The caller omits the target when read-side policy hides it.
pub fn build_ref_advertisement_typed<'a, I>(
    refs: I,
    head_symref_target: Option<&str>,
) -> Result<String>
where
    I: IntoIterator<Item = &'a Reference>,
{
    let _span = crate::gix_boundary!("ref", "advertise_typed").entered();

    use std::fmt::Write;
    let mut buf = String::new();

    if let Some(target) = head_symref_target {
        writeln!(buf, "@{target} HEAD")
            .map_err(|e| CrabError::Internal(format!("format list output: {e}")))?;
    }

    for reference in refs {
        // Resolve `Reference::target` to a concrete object id. For
        // typed symbolic refs we skip — the helper-protocol `list`
        // response only emits concrete `{sha} {ref_name}` lines; the
        // `@{target} HEAD` line above already carries the symbolic
        // information clients need. Matches what `format_list_output`
        // does with the legacy `RefEntry` shape, where the resolver
        // flattens HEAD into a concrete SHA before the format call.
        let oid_hex = match &reference.target {
            gix_ref::Target::Object(oid) => oid.to_hex().to_string(),
            gix_ref::Target::Symbolic(_) => continue,
        };
        let name = reference.name.as_bstr();
        let name_str = std::str::from_utf8(name)
            .map_err(|e| CrabError::Protocol(format!("ref name is not UTF-8: {e}")))?;
        writeln!(buf, "{oid_hex} {name_str}")
            .map_err(|e| CrabError::Internal(format!("format list output: {e}")))?;
    }
    buf.push('\n');
    Ok(buf)
}

// --- 5.3 Negotiation scaffold ---------------------------------------

/// Construct a `gix_negotiate::Algorithm` state machine for compatibility
/// callers that still use the released client-side API.
///
/// The production local upload-pack path owns negotiation in
/// `crate::git::upload_pack_wire` and `crab-read`; this adapter is not a
/// second fetch implementation.
pub fn negotiate_session(
    algorithm: gix_negotiate::Algorithm,
) -> Box<dyn gix_negotiate::Negotiator> {
    let _span = crate::gix_boundary!("negotiate", "session").entered();
    algorithm.into_negotiator()
}

// --- 5.6 Replace `remote get-url origin` with gix ------------------

/// Look up the `origin` remote's fetch URL via `gix::Repository`.
///
/// Drop-in replacement for the `std::process::Command::new("git")
/// .args(["remote", "get-url", "origin"])` shellout in
/// [`crate::git::remote_helper::fetch_packs`]. Returns `Ok(None)` when
/// the repo has no `origin` remote configured — the legacy shellout
/// returned an empty string in that case, which the caller treated as
/// "no URL". Callers preserve the exact same fall-through behavior by
/// checking for `None`.
///
/// `repo_path` is the path to any directory inside the working tree;
/// `gix::discover` walks up to find the git dir, mirroring what
/// `git remote get-url origin` does when invoked from a subdirectory.
pub fn remote_origin_url(repo_path: &std::path::Path) -> Result<Option<String>> {
    let _span = crate::gix_boundary!("repo", "find_remote_origin").entered();

    let repo = gix::discover(repo_path)
        .map_err(|e| CrabError::Internal(format!("gix::discover failed: {e}")))?;

    // `find_remote` errors when the remote doesn't exist; map that to
    // `None` to match the shellout's empty-string semantics rather
    // than escalating a missing remote to a hard error. Callers that
    // need the richer shape can read the error chain if this ever
    // needs to grow into an explicit `NotFound` variant.
    let remote = match repo.find_remote("origin") {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "origin remote not configured");
            return Ok(None);
        }
    };

    let url = remote.url(gix::remote::Direction::Fetch);
    Ok(url.map(|u| u.to_bstring().to_string()))
}

// --- 5.1 StdioTransport ---------------------------------------------

/// Retained released client transport API; use the remote helper's local
/// upload-pack session for protocol-v2 fetches.
///
/// This type is deprecated for one release cycle. Its I/O methods fail closed
/// so a caller cannot accidentally create a second, role-inverted fetch path.
///
/// Generic over the reader and writer so tests can swap in in-memory
/// buffers instead of real stdin/stdout. The blanket impl assumes
/// the reader yields git protocol-v2 packet-line payloads on demand;
/// it does not own any additional framing state.
#[deprecated(
    note = "use the local upload-pack session in git::upload_pack_wire; this client transport is retained only for compatibility"
)]
pub struct StdioTransport<R: BufRead, W: Write> {
    /// Canonical URL stored so [`TransportWithoutIO::to_url`] can
    /// return it without re-allocating. Built once at construction
    /// from the `crab://...` URL the helper was invoked with.
    url: BString,
    /// Reader side of the stateless-connect pipe. In production this
    /// is `BufReader<Stdin>`; tests wrap a `Cursor<Vec<u8>>`.
    #[allow(dead_code)]
    reader: R,
    /// Writer side of the stateless-connect pipe. In production this
    /// is `Stdout`; tests take a `Vec<u8>`.
    #[allow(dead_code)]
    writer: W,
}

impl<R: BufRead, W: Write> StdioTransport<R, W> {
    /// Build a transport over an arbitrary reader/writer pair.
    ///
    /// This constructor is retained for compatibility tests and integrations.
    pub fn new(url: impl Into<BString>, reader: R, writer: W) -> Self {
        Self {
            url: url.into(),
            reader,
            writer,
        }
    }
}

impl StdioTransport<BufReader<Stdin>, Stdout> {
    /// Build the retained compatibility transport over process stdin/stdout.
    ///
    /// `url` is the canonical `crab://bucket/repo` URL the remote
    /// helper was invoked with; it is only used to answer
    /// [`TransportWithoutIO::to_url`]. Stateless-connect does not
    /// carry the URL in-band, so this is purely for diagnostics and
    /// for the transport's self-identification in higher-level
    /// gitoxide traces.
    pub fn from_stdio(url: impl Into<BString>) -> Self {
        Self::new(url, BufReader::new(std::io::stdin()), std::io::stdout())
    }
}

impl<R: BufRead, W: Write> TransportWithoutIO for StdioTransport<R, W> {
    fn to_url(&self) -> Cow<'_, BStr> {
        Cow::Borrowed(self.url.as_bstr())
    }

    fn supported_protocol_versions(&self) -> &[Protocol] {
        // Crab only ever intends to drive protocol v2 over
        // stateless-connect; v1 stays on the existing pack-download
        // path because of the `gix-protocol` v1-stateful hang
        // mentioned in gitoxide's SHORTCOMINGS.md.
        &[Protocol::V2]
    }

    fn connection_persists_across_multiple_requests(&self) -> bool {
        // Stateless-connect, by definition, treats each request as
        // independent. Returning `false` matches the HTTP transport's
        // behavior, which is the nearest analog to the stdio pipe
        // the remote helper drives.
        false
    }

    fn configure(
        &mut self,
        _config: &dyn Any,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        // The compatibility trait has no configuration knobs used by the
        // local upload-pack session, so retain the released no-op contract.
        Ok(())
    }
}

/// The `Transport` impl remains split from `TransportWithoutIO` to preserve
/// the released API shape. Neither method is used by production fetches; both
/// return the dependency's explicit unsupported-operation error.
impl<R: BufRead + Send, W: Write + Send> BlockingTransport for StdioTransport<R, W> {
    fn handshake<'a>(
        &mut self,
        _service: gix_transport::Service,
        _extra_parameters: &'a [(&'a str, Option<&'a str>)],
    ) -> std::result::Result<SetServiceResponse<'_>, TransportError> {
        // Git is already the protocol client after terminal takeover. The
        // production helper therefore cannot route this client API back into
        // the same stream without reversing the protocol roles.
        Err(TransportError::AuthenticationUnsupported)
    }

    fn request(
        &mut self,
        _write_mode: WriteMode,
        _on_into_read: MessageKind,
        _trace: bool,
    ) -> std::result::Result<RequestWriter<'_>, TransportError> {
        Err(TransportError::AuthenticationUnsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // --- 5.1 StdioTransport smoke tests ---

    #[test]
    fn stdio_transport_reports_url_without_io() {
        let reader = Cursor::new(Vec::<u8>::new());
        let writer = Vec::<u8>::new();
        let t = StdioTransport::new(
            BString::from("crab://bucket/repo"),
            BufReader::new(reader),
            writer,
        );
        assert_eq!(t.to_url().as_ref(), b"crab://bucket/repo".as_bstr());
    }

    #[test]
    fn stdio_transport_supports_only_v2() {
        let reader = Cursor::new(Vec::<u8>::new());
        let writer = Vec::<u8>::new();
        let t = StdioTransport::new(
            BString::from("crab://bucket/repo"),
            BufReader::new(reader),
            writer,
        );
        assert_eq!(t.supported_protocol_versions(), &[Protocol::V2]);
    }

    #[test]
    fn stdio_transport_is_stateless() {
        let reader = Cursor::new(Vec::<u8>::new());
        let writer = Vec::<u8>::new();
        let t = StdioTransport::new(
            BString::from("crab://bucket/repo"),
            BufReader::new(reader),
            writer,
        );
        assert!(!t.connection_persists_across_multiple_requests());
    }

    #[test]
    fn stdio_transport_configure_is_noop() {
        let reader = Cursor::new(Vec::<u8>::new());
        let writer = Vec::<u8>::new();
        let mut t = StdioTransport::new(
            BString::from("crab://bucket/repo"),
            BufReader::new(reader),
            writer,
        );
        t.configure(&()).expect("configure should succeed");
    }

    // --- 5.4 Refspec parse tests ---

    #[test]
    fn parse_matching_push_refspec() {
        let spec =
            parse_refspec_gix("refs/heads/main:refs/heads/main").expect("parse should succeed");
        assert!(!spec.force);
        assert_eq!(spec.src, "refs/heads/main");
        assert_eq!(spec.dst, "refs/heads/main");
    }

    #[test]
    fn parse_force_push_refspec() {
        let spec =
            parse_refspec_gix("+refs/heads/main:refs/heads/main").expect("parse should succeed");
        assert!(spec.force);
        assert_eq!(spec.src, "refs/heads/main");
        assert_eq!(spec.dst, "refs/heads/main");
    }

    #[test]
    fn parse_delete_push_refspec() {
        let spec = parse_refspec_gix(":refs/heads/dead-branch").expect("parse should succeed");
        assert!(!spec.force);
        assert_eq!(spec.src, "");
        assert_eq!(spec.dst, "refs/heads/dead-branch");
    }

    #[test]
    fn parse_rejects_malformed_refspec() {
        let err = parse_refspec_gix("").expect_err("empty refspec should not parse");
        assert!(matches!(err, CrabError::Protocol(_)));
    }

    // --- 5.2 Typed ref advertisement tests ---

    #[test]
    fn typed_ref_advertisement_matches_legacy_shape() {
        use gix_hash::ObjectId;
        use gix_ref::{FullName, Target};

        // Construct two concrete refs the same way resolve_refs_typed_batch
        // would hand them back. `FullName::try_from` and
        // `ObjectId::from_bytes_or_panic` (via the gix helper) would
        // both be hit in production; here we go straight through the
        // owned-reference constructor.
        let main_oid = ObjectId::from_hex(b"abc123def456abc123def456abc123def456abcd").unwrap();
        let dev_oid = ObjectId::from_hex(b"111222333444555666777888999000aaabbbcccd").unwrap();

        let main_ref = Reference {
            name: FullName::try_from("refs/heads/main").unwrap(),
            target: Target::Object(main_oid),
            peeled: None,
        };
        let dev_ref = Reference {
            name: FullName::try_from("refs/heads/dev").unwrap(),
            target: Target::Object(dev_oid),
            peeled: None,
        };

        let out = build_ref_advertisement_typed([&main_ref, &dev_ref], Some("refs/heads/main"))
            .expect("format should succeed");

        assert_eq!(
            out,
            "@refs/heads/main HEAD\n\
             abc123def456abc123def456abc123def456abcd refs/heads/main\n\
             111222333444555666777888999000aaabbbcccd refs/heads/dev\n\
             \n"
        );
    }

    #[test]
    fn typed_ref_advertisement_skips_symbolic_targets() {
        use gix_ref::{FullName, Target};

        // A symbolic target should be skipped (the @-line captures
        // HEAD; concrete lines only emit object targets).
        let sym_ref = Reference {
            name: FullName::try_from("HEAD").unwrap(),
            target: Target::Symbolic(FullName::try_from("refs/heads/main").unwrap()),
            peeled: None,
        };

        let out = build_ref_advertisement_typed([&sym_ref], None).expect("format should succeed");
        assert_eq!(out, "\n");
    }

    // --- 5.3 Negotiation scaffold tests ---

    #[test]
    fn negotiate_session_constructs_without_panic() {
        // Exercises the gix-negotiate boxed constructor for each
        // algorithm. Proves the crate is compiled in and the API is
        // reachable under the `gix-transport` feature.
        let _noop = negotiate_session(gix_negotiate::Algorithm::Noop);
        let _consecutive = negotiate_session(gix_negotiate::Algorithm::Consecutive);
        let _skipping = negotiate_session(gix_negotiate::Algorithm::Skipping);
    }

    // --- 5.6 remote_origin_url tests ---

    #[test]
    fn remote_origin_url_on_missing_origin_returns_none() {
        // Build a bare git-discoverable directory that has no
        // `origin` remote; the helper should fall through to `None`
        // rather than erroring.
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = gix::init_bare(tmp.path()).expect("gix::init_bare");
        drop(repo);

        let url = remote_origin_url(tmp.path()).expect("should not error without origin");
        assert!(url.is_none());
    }
}
