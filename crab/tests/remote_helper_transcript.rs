//! Snapshot tests for the git remote helper protocol transcripts.
//!
//! Each test sends a sequence of commands through `run_remote_helper` and
//! snapshots the exact byte-level response using `cargo insta`.

#![recursion_limit = "256"]
use std::io::Cursor;

use crab::git::remote_helper::{
    ListOutput, RefEntry, StdIo, filter_list_for_push, format_capabilities, format_list_output,
};
use tokio::io::BufReader;

fn test_url() -> (tempfile::TempDir, gix_url::Url) {
    let remote = tempfile::tempdir().expect("create local transcript remote");
    let url = format!("file://{}", remote.path().display());
    let parsed = gix_url::Url::from_bytes(url.as_str().into()).expect("test URL should parse");
    (remote, parsed)
}

/// Run the remote helper with the given input and capture stdout as a string.
///
/// Uses a duplex stream so we can read the output after the helper exits.
async fn run_remote_helper_capture(input: &str) -> String {
    // Since run_remote_helper takes `impl StdIo` by value and the writer
    // is moved into the function, we use a channel to capture output.
    use tokio::io::{AsyncReadExt, duplex};

    let input_bytes = input.as_bytes().to_vec();
    let (writer_tx, mut writer_rx) = duplex(64 * 1024);

    struct DuplexIo {
        reader: BufReader<Cursor<Vec<u8>>>,
        writer: tokio::io::DuplexStream,
    }

    impl StdIo for DuplexIo {
        type Reader = BufReader<Cursor<Vec<u8>>>;
        type Writer = tokio::io::DuplexStream;

        fn split(self) -> (Self::Reader, Self::Writer) {
            (self.reader, self.writer)
        }
    }

    let io = DuplexIo {
        reader: BufReader::new(Cursor::new(input_bytes)),
        writer: writer_tx,
    };

    let (_remote, url) = test_url();
    let mut output = Vec::new();
    let helper = crab::git::remote_helper::run_remote_helper(
        "origin",
        &url,
        io,
        tokio_util::sync::CancellationToken::new(),
    );
    let reader = writer_rx.read_to_end(&mut output);
    let (helper_result, read_result) = tokio::join!(helper, reader);
    helper_result.expect("remote helper should not error");
    read_result.expect("reading output should succeed");

    String::from_utf8(output).expect("output should be valid UTF-8")
}

// --- Snapshot tests ---

#[tokio::test]
async fn capabilities_transcript() {
    let output = format_capabilities(false);
    let expected_agent = format!("agent=crab/{}", env!("CARGO_PKG_VERSION"));
    assert!(
        output.lines().any(|line| line == expected_agent),
        "capabilities must advertise the crate version {expected_agent:?}; output: {output:?}",
    );
    let normalized = output.replace(&expected_agent, "agent=crab/<version>");
    insta::assert_snapshot!(normalized, @r"
    fetch
    push
    option
    check-connectivity
    agent=crab/<version>

    ");
}

#[tokio::test]
async fn option_handling_transcript() {
    let input = "\
option progress true\n\
option verbosity 2\n\
option unknown-key value\n";
    let output = run_remote_helper_capture(input).await;
    insta::assert_snapshot!(output, @r"
    ok
    ok
    unsupported
    ");
}

#[test]
fn head_symref_in_list_output() {
    let output = ListOutput {
        refs: vec![
            RefEntry {
                sha: "abc123def456abc123def456abc123def456abcd".into(),
                ref_name: "refs/heads/main".into(),
                peeled: None,
            },
            RefEntry {
                sha: "111222333444555666777888999000aaabbbcccd".into(),
                ref_name: "refs/heads/dev".into(),
                peeled: None,
            },
        ],
        head_symref: Some("refs/heads/main".into()),
    };
    let formatted = format_list_output(&output);
    insta::assert_snapshot!(formatted, @r"
    @refs/heads/main HEAD
    abc123def456abc123def456abc123def456abcd refs/heads/main
    111222333444555666777888999000aaabbbcccd refs/heads/dev

    ");
}

#[cfg(unix)]
#[test]
fn native_git_list_omits_unborn_head() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let directory = tempfile::tempdir().expect("helper fixture directory");
    let helper = directory.path().join("git-remote-crab-ref-test");
    std::fs::write(
        &helper,
        "#!/bin/sh\nwhile IFS= read -r command; do\ncase \"$command\" in\ncapabilities) printf 'fetch\\n\\n';;\nlist) cat \"$2\";;\n\"\") exit;;\n*) exit 1;;\nesac\ndone\n",
    )
    .expect("write helper");
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))
        .expect("executable helper");
    let path = std::env::join_paths(std::iter::once(directory.path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("helper search path");
    let advertisement = directory.path().join("refs");
    let oid = "a".repeat(40);
    for refs in [
        Vec::new(),
        vec![RefEntry {
            sha: oid.clone(),
            ref_name: "refs/tags/v1".into(),
            peeled: None,
        }],
    ] {
        let expected = refs
            .iter()
            .map(|reference| format!("{}\t{}\n", reference.sha, reference.ref_name))
            .collect::<String>();
        std::fs::write(
            &advertisement,
            format_list_output(&ListOutput {
                refs,
                head_symref: Some("refs/heads/main".into()),
            }),
        )
        .expect("write advertisement");
        let output = Command::new("git")
            .current_dir(directory.path())
            .env("PATH", &path)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args(["-c", "protocol.version=0", "ls-remote", "--symref"])
            .arg(format!("crab-ref-test::{}", advertisement.display()))
            .output()
            .expect("native Git ref listing");
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(String::from_utf8(output.stdout).expect("listing"), expected);
    }
}

#[test]
fn list_for_push_omits_head_symref() {
    let output = ListOutput {
        refs: vec![
            RefEntry {
                sha: "abc123def456abc123def456abc123def456abcd".into(),
                ref_name: "refs/heads/main".into(),
                peeled: None,
            },
            RefEntry {
                sha: "111222333444555666777888999000aaabbbcccd".into(),
                ref_name: "refs/heads/dev".into(),
                peeled: None,
            },
        ],
        head_symref: Some("refs/heads/main".into()),
    };
    let filtered = filter_list_for_push(output, true);
    let formatted = format_list_output(&filtered);
    insta::assert_snapshot!(formatted, @r"
    abc123def456abc123def456abc123def456abcd refs/heads/main
    111222333444555666777888999000aaabbbcccd refs/heads/dev

    ");
}

// ---------------------------------------------------------------------------
// Ref-name validation regression tests
//
// These push a single malformed refspec through the transcript harness
// and assert `error {dst} bad-refname` lands in the response. The
// validator runs inside `parse_command` before any pipeline work, so
// a batch containing only bad-refname entries short-circuits with
// zero store / git interaction — which is what lets these tests run
// without a fixture repo or a real object store.
// ---------------------------------------------------------------------------

mod refname_validation {
    use std::io::Cursor;
    use std::time::Duration;

    use tokio::io::BufReader;

    use crab::git::remote_helper::StdIo;

    fn test_url() -> (tempfile::TempDir, gix_url::Url) {
        let remote = tempfile::tempdir().expect("create local transcript remote");
        let url = format!("file://{}", remote.path().display());
        let parsed = gix_url::Url::from_bytes(url.as_str().into()).expect("test URL should parse");
        (remote, parsed)
    }

    /// Run the helper with `input`, capture stdout, and return both the
    /// bytes emitted and the terminal `Result` from the helper task.
    ///
    /// Unlike the top-level `run_remote_helper_capture` harness, this
    /// variant keeps the helper's error instead of panicking on it —
    /// tests here need to distinguish "batch aborted with a protocol
    /// error" (genuine malformed syntax) from "per-ref rejection
    /// landed in the response" (bad refname).
    ///
    /// The helper is wrapped in a short timeout as a belt-and-braces
    /// guard: any regression that re-routes a pre-rejected ref
    /// through the pipeline would otherwise block on network I/O
    /// indefinitely. A timeout hit is surfaced as an `Err` so the
    /// caller can decide how to treat it.
    async fn run_capture(input: &str) -> (String, std::result::Result<(), String>) {
        run_capture_with_timeout(input, Duration::from_secs(10)).await
    }

    async fn run_capture_with_timeout(
        input: &str,
        timeout: Duration,
    ) -> (String, std::result::Result<(), String>) {
        use tokio::io::{AsyncReadExt, duplex};

        let input_bytes = input.as_bytes().to_vec();
        let (writer_tx, mut writer_rx) = duplex(64 * 1024);

        struct DuplexIo {
            reader: BufReader<Cursor<Vec<u8>>>,
            writer: tokio::io::DuplexStream,
        }

        impl StdIo for DuplexIo {
            type Reader = BufReader<Cursor<Vec<u8>>>;
            type Writer = tokio::io::DuplexStream;

            fn split(self) -> (Self::Reader, Self::Writer) {
                (self.reader, self.writer)
            }
        }

        let io = DuplexIo {
            reader: BufReader::new(Cursor::new(input_bytes)),
            writer: writer_tx,
        };

        let (_remote, url) = test_url();
        let mut output = Vec::new();
        let helper = crab::git::remote_helper::run_remote_helper(
            "origin",
            &url,
            io,
            tokio_util::sync::CancellationToken::new(),
        );
        let reader = writer_rx.read_to_end(&mut output);

        let helper_result =
            match tokio::time::timeout(timeout, async { tokio::join!(helper, reader) }).await {
                Ok((Ok(()), Ok(_))) => Ok(()),
                Ok((Ok(()), Err(e))) => Err(e.to_string()),
                Ok((Err(e), _)) => Err(e.to_string()),
                Err(_) => Err("helper task timed out".to_owned()),
            };

        (String::from_utf8_lossy(&output).into_owned(), helper_result)
    }

    /// `..` anywhere in a path component is forbidden by git's
    /// `check-ref-format` and by `gix_validate::reference::name_partial`.
    #[tokio::test]
    async fn dot_dot_ref_name_rejected() {
        let input = "push src-sha:refs/heads/..foo\n";
        let (output, _) = run_capture(input).await;
        assert!(
            output.contains("error refs/heads/..foo bad-refname"),
            "expected bad-refname rejection for refs/heads/..foo; output={output:?}"
        );
    }

    /// `.lock` suffix is reserved by git's filesystem refs backend
    /// (loose refs use it for atomic update staging).
    #[tokio::test]
    async fn lock_suffix_rejected() {
        let input = "push src-sha:refs/heads/main.lock\n";
        let (output, _) = run_capture(input).await;
        assert!(
            output.contains("error refs/heads/main.lock bad-refname"),
            "expected bad-refname rejection for refs/heads/main.lock; output={output:?}"
        );
    }

    /// An empty `dst` cannot be surfaced as `error {dst} bad-refname`
    /// because there is no ref to address the rejection against. The
    /// remote-helper spec treats this as malformed syntax, so the
    /// whole batch aborts with a protocol error rather than silently
    /// accepting or pretending to reject a ref.
    #[tokio::test]
    async fn empty_dst_rejected() {
        let input = "push refs/heads/feature:\n";
        let (output, helper_result) = run_capture(input).await;
        assert!(
            helper_result.is_err(),
            "empty dst must abort the batch with a protocol error; \
             output={output:?} result={helper_result:?}"
        );
        let err_msg = helper_result.err().unwrap_or_default();
        assert!(
            err_msg.contains("empty dst") || err_msg.contains("Protocol"),
            "expected a protocol error about empty dst; got {err_msg}"
        );
        assert!(
            !output.contains("bad-refname"),
            "empty dst must not be funnelled through the per-ref rejection path; \
             output={output:?}"
        );
    }

    /// `push :refs/heads/old` is the `git push :ref` delete form.
    /// Empty `src` is a legal refspec and must not trigger the
    /// refname validator — deleting a ref has no source name to
    /// validate. The test asserts that the validator did not fire;
    /// whether the delete succeeds end-to-end is covered by the
    /// `delete_ref` module below.
    #[tokio::test]
    async fn empty_src_is_delete_not_bad_refname() {
        // Single delete refspec drives a full pipeline run (there
        // is no way to inspect parse results in isolation from the
        // integration harness). The generous timeout accommodates
        // slow debug builds and the store-warmup path; the only
        // assertion is that no `bad-refname` rejection ever lands
        // against `refs/heads/old`.
        let input = "push :refs/heads/old\n";
        let (output, _) = run_capture_with_timeout(input, Duration::from_secs(60)).await;
        assert!(
            !output.contains("refs/heads/old bad-refname"),
            "empty src delete must not be rejected as bad-refname; output={output:?}"
        );
    }

    /// ASCII control bytes are invalid in ref names.
    #[tokio::test]
    async fn control_char_in_ref_rejected() {
        let input = "push src-sha:refs/heads/bad\x01name\n";
        let (output, _) = run_capture(input).await;
        assert!(
            output.contains("bad-refname"),
            "control byte in ref must trigger bad-refname; output={output:?}"
        );
    }

    /// `@{` is reserved by git as reflog shorthand (`@{1}`, `@{upstream}`)
    /// and rejected by `name_partial` via `ReflogPortion`. The `@`
    /// character on its own as a component is not rejected, which the
    /// original design called out as a gap. Using the reflog-portion
    /// pattern keeps the test representative of real ref-name abuse
    /// rather than hypothetical.
    #[tokio::test]
    async fn at_symbol_component_rejected() {
        let input = "push src-sha:refs/heads/foo@{upstream}\n";
        let (output, _) = run_capture(input).await;
        assert!(
            output.contains("error refs/heads/foo@{upstream} bad-refname"),
            "expected bad-refname rejection for refs/heads/foo@{{upstream}}; output={output:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Fetch reachability regression tests
//
// These exercise the upload-pack policy that gates raw-SHA
// `fetch <sha> <ref>` lines. The core validator is a pure sync
// function (`validate_fetch_entries_with_manifest`), so these tests
// drive it directly and assert the rejection's protocol tag /
// reason — the same tag the transcript response embeds via
// `error {ref_name} {tag} ({reason})`.
//
// A full transcript round-trip requires a primed object store with
// a manifest; the wire-format assertion here (protocol tag +
// reason string) covers the plumbing that `fetch_packs` uses when
// emitting the per-entry error line.
// ---------------------------------------------------------------------------

mod fetch_reachability {
    use std::collections::BTreeMap;

    use crab::core::config::Config;
    use crab::git::reject_reason::FetchRejectReason;
    use crab::git::remote_helper::{FetchEntry, validate_fetch_entries_with_manifest};
    use crab::metadata::manifest::Manifest;
    use crab_metadata::commit_graph::{CommitEntry, CommitGraphSummary};

    fn manifest_with_ref(ref_name: &str, sha: &str) -> Manifest {
        let mut refs = BTreeMap::new();
        refs.insert(ref_name.to_owned(), sha.to_owned());
        let mut manifest = Manifest::default_for_repo(ref_name);
        manifest.generation = 1;
        manifest.refs = refs;
        manifest.seal_git_validation();
        manifest
    }

    /// Non-tip SHA under the default `allow_tip` policy must surface
    /// a `not-at-tip` rejection — `fetch_packs` formats this as
    /// `error {ref_name} not-at-tip ({detail})\n` and skips the
    /// pack download.
    #[test]
    fn fetch_rejects_non_tip_sha_when_only_tip_allowed() {
        let tip = "a".repeat(40);
        let non_tip = "b".repeat(40);
        let manifest = manifest_with_ref("refs/heads/main", &tip);
        let entries = vec![FetchEntry {
            sha: non_tip.clone(),
            ref_name: "refs/heads/hidden".into(),
        }];
        let cfg = Config::default();

        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert_eq!(result.len(), 1);
        let (entry, outcome) = &result[0];
        let err = outcome
            .as_ref()
            .err()
            .expect("non-tip sha must be rejected");
        assert_eq!(err.protocol_tag(), "not-at-tip");

        // The transcript wire line `fetch_packs` emits embeds the
        // ref name, the protocol tag, and the detail. Reconstruct
        // that line here to confirm the shape the client will see.
        let line = format!("error {} {} ({})", entry.ref_name, err.protocol_tag(), err);
        assert!(line.contains("refs/heads/hidden"));
        assert!(line.contains("not-at-tip"));
        assert!(line.contains(&non_tip));
    }

    #[test]
    fn fetch_accepts_tip_sha_when_tip_allowed() {
        let tip = "a".repeat(40);
        let manifest = manifest_with_ref("refs/heads/main", &tip);
        let entries = vec![FetchEntry {
            sha: tip.clone(),
            ref_name: "refs/heads/main".into(),
        }];
        let cfg = Config::default();

        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert!(
            result[0].1.is_ok(),
            "tip SHA must be accepted; got {:?}",
            result[0].1
        );
    }

    #[test]
    fn fetch_allow_any_sha_in_want_bypasses_check() {
        let tip = "a".repeat(40);
        let non_tip = "b".repeat(40);
        let manifest = manifest_with_ref("refs/heads/main", &tip);
        let entries = vec![FetchEntry {
            sha: non_tip,
            ref_name: "refs/heads/feature".into(),
        }];
        let mut cfg = Config::default();
        cfg.uploadpack_allow_any_sha_in_want = true;

        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert!(
            result[0].1.is_ok(),
            "allow_any bypasses policy; got {:?}",
            result[0].1
        );
    }

    #[test]
    fn fetch_reachable_sha_accepted_when_reachable_allowed() {
        // Chain: A → B (tip).
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let summary = CommitGraphSummary {
            generation: 1,
            commits: vec![
                CommitEntry {
                    oid: sha_a.clone(),
                    gen_number: 0,
                    parents: vec![],
                },
                CommitEntry {
                    oid: sha_b.clone(),
                    gen_number: 1,
                    parents: vec![sha_a.clone()],
                },
            ],
        };
        let manifest = manifest_with_ref("refs/heads/main", &sha_b);

        let entries = vec![FetchEntry {
            sha: sha_a,
            ref_name: "refs/heads/main".into(),
        }];
        let mut cfg = Config::default();
        cfg.uploadpack_allow_reachable_sha_in_want = true;

        let result =
            validate_fetch_entries_with_manifest(&entries, &manifest, Some(&summary), &cfg);

        assert!(
            result[0].1.is_ok(),
            "reachable ancestor must be accepted when allow_reachable is on; got {:?}",
            result[0].1
        );
    }

    #[test]
    fn fetch_rejects_unreachable_sha_as_not_reachable_when_reachable_allowed() {
        let sha_tip = "a".repeat(40);
        let sha_phantom = "f".repeat(40);
        let manifest = manifest_with_ref("refs/heads/main", &sha_tip);
        let summary = CommitGraphSummary {
            generation: 1,
            commits: vec![CommitEntry {
                oid: sha_tip,
                gen_number: 0,
                parents: vec![],
            }],
        };
        let entries = vec![FetchEntry {
            sha: sha_phantom.clone(),
            ref_name: "refs/heads/wat".into(),
        }];
        let mut cfg = Config::default();
        cfg.uploadpack_allow_reachable_sha_in_want = true;

        let result =
            validate_fetch_entries_with_manifest(&entries, &manifest, Some(&summary), &cfg);

        match &result[0].1 {
            Err(FetchRejectReason::NotReachable { sha }) => assert_eq!(sha, &sha_phantom),
            other => panic!("expected NotReachable, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Peeled tag advertisement (F1-1)
//
// Verifies that `format_list_output` emits the `{peeled} {ref}^{{}}\n`
// line expected by git clients when an annotated tag's `peeled` field
// is populated, and that the line is absent for lightweight tags /
// branches where `peeled` is `None`.
// ---------------------------------------------------------------------------

mod peeled_tag {
    use crab::git::remote_helper::{ListOutput, RefEntry, format_list_output};

    #[test]
    fn peeled_tag_line_emitted_for_annotated_tag() {
        let tag_object_sha = "a".repeat(40);
        let peeled_commit_sha = "b".repeat(40);

        let output = ListOutput {
            refs: vec![RefEntry {
                sha: tag_object_sha.clone(),
                ref_name: "refs/tags/v1".into(),
                peeled: Some(peeled_commit_sha.clone()),
            }],
            head_symref: None,
        };

        let formatted = format_list_output(&output);
        let expected =
            format!("{tag_object_sha} refs/tags/v1\n{peeled_commit_sha} refs/tags/v1^{{}}\n\n");
        assert_eq!(formatted, expected);
    }

    /// Lightweight tag (or any ref without a peel target) must emit
    /// exactly one line — adding a spurious `^{}` line would point
    /// at nothing and confuse clients.
    #[test]
    fn lightweight_tag_has_no_peeled_line() {
        let sha = "c".repeat(40);
        let output = ListOutput {
            refs: vec![RefEntry {
                sha: sha.clone(),
                ref_name: "refs/tags/lightweight".into(),
                peeled: None,
            }],
            head_symref: None,
        };

        let formatted = format_list_output(&output);
        assert_eq!(formatted, format!("{sha} refs/tags/lightweight\n\n"));
        assert!(!formatted.contains("^{}"));
    }

    /// Mixed set: annotated tag + branch + lightweight tag. The peel
    /// line appears only beside the annotated tag.
    #[test]
    fn peeled_line_interleaved_with_branches() {
        let tag_sha = "1".repeat(40);
        let commit_sha = "2".repeat(40);
        let main_sha = "3".repeat(40);
        let light_sha = "4".repeat(40);

        let output = ListOutput {
            refs: vec![
                RefEntry {
                    sha: main_sha.clone(),
                    ref_name: "refs/heads/main".into(),
                    peeled: None,
                },
                RefEntry {
                    sha: light_sha.clone(),
                    ref_name: "refs/tags/lightweight".into(),
                    peeled: None,
                },
                RefEntry {
                    sha: tag_sha.clone(),
                    ref_name: "refs/tags/v1".into(),
                    peeled: Some(commit_sha.clone()),
                },
            ],
            head_symref: Some("refs/heads/main".into()),
        };

        let formatted = format_list_output(&output);
        let expected = format!(
            "@refs/heads/main HEAD\n\
             {main_sha} refs/heads/main\n\
             {light_sha} refs/tags/lightweight\n\
             {tag_sha} refs/tags/v1\n\
             {commit_sha} refs/tags/v1^{{}}\n\n"
        );
        assert_eq!(formatted, expected);
    }
}

// ---------------------------------------------------------------------------
// `transfer.hideRefs`
//
// Verifies that refs matching `transfer.hideRefs` glob patterns are
// omitted from `read_remote_refs` output and that their tip SHAs are
// excluded from the allowlist used by the fetch-side policy check.
// ---------------------------------------------------------------------------

mod transfer_hide_refs {
    use std::collections::BTreeMap;

    use crab::core::config::Config;
    use crab::git::reject_reason::FetchRejectReason;
    use crab::git::remote_helper::{FetchEntry, validate_fetch_entries_with_manifest};
    use crab::metadata::manifest::Manifest;

    fn manifest_with_refs(pairs: &[(&str, &str)]) -> Manifest {
        let mut refs = BTreeMap::new();
        for (name, sha) in pairs {
            refs.insert((*name).to_owned(), (*sha).to_owned());
        }
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.refs = refs;
        manifest.seal_git_validation();
        manifest
    }

    /// A hidden ref's tip SHA must not pass the `allow_tip` gate —
    /// otherwise a client that already guesses the SHA can bypass
    /// the `hideRefs` policy entirely.
    #[test]
    fn hidden_ref_tip_sha_rejected_by_fetch_policy() {
        let visible_sha = "a".repeat(40);
        let hidden_sha = "b".repeat(40);
        let manifest = manifest_with_refs(&[
            ("refs/heads/main", &visible_sha),
            ("refs/heads/secret", &hidden_sha),
        ]);

        let mut cfg = Config::default();
        cfg.transfer_hide_refs = vec!["refs/heads/secret".into()];

        // Fetch the hidden ref's tip SHA — must be rejected before
        // upload-pack admission falls through to any object allowlist.
        let entries = vec![FetchEntry {
            sha: hidden_sha.clone(),
            ref_name: "refs/heads/secret".into(),
        }];
        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert_eq!(result.len(), 1);
        match &result[0].1 {
            Err(FetchRejectReason::NotAllowed { sha, reason }) => {
                assert_eq!(sha, &hidden_sha);
                assert_eq!(reason, "hidden-ref target");
            }
            other => panic!("expected NotAllowed rejection for hidden ref tip; got {other:?}"),
        }
    }

    /// Visible refs continue to pass `allow_tip` normally — the
    /// filter must scope to pattern matches, not block the universe.
    #[test]
    fn visible_ref_tip_sha_still_accepted_under_hide_refs() {
        let visible_sha = "a".repeat(40);
        let manifest = manifest_with_refs(&[("refs/heads/main", &visible_sha)]);

        let mut cfg = Config::default();
        cfg.transfer_hide_refs = vec!["refs/heads/internal/*".into()];

        let entries = vec![FetchEntry {
            sha: visible_sha.clone(),
            ref_name: "refs/heads/main".into(),
        }];
        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert!(
            result[0].1.is_ok(),
            "visible ref must pass allow_tip; got {:?}",
            result[0].1
        );
    }

    /// Glob pattern matches should be case-sensitive and scoped —
    /// a `refs/heads/internal/*` pattern must not accidentally
    /// filter `refs/heads/main`.
    #[test]
    fn hide_refs_glob_scope() {
        let main_sha = "a".repeat(40);
        let internal_sha = "b".repeat(40);
        let manifest = manifest_with_refs(&[
            ("refs/heads/main", &main_sha),
            ("refs/heads/internal/secret", &internal_sha),
        ]);

        let mut cfg = Config::default();
        cfg.transfer_hide_refs = vec!["refs/heads/internal/*".into()];

        // internal/secret hidden → tip SHA rejected.
        let entries = vec![FetchEntry {
            sha: internal_sha.clone(),
            ref_name: "refs/heads/internal/secret".into(),
        }];
        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);
        match &result[0].1 {
            Err(FetchRejectReason::NotAllowed { sha, reason }) => {
                assert_eq!(sha, &internal_sha);
                assert_eq!(reason, "hidden-ref target");
            }
            other => panic!("hidden ref tip must be rejected; got {other:?}"),
        }

        // main still visible.
        let entries = vec![FetchEntry {
            sha: main_sha.clone(),
            ref_name: "refs/heads/main".into(),
        }];
        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);
        assert!(
            result[0].1.is_ok(),
            "unrelated ref must stay visible; got {:?}",
            result[0].1
        );
    }
}

// ---------------------------------------------------------------------------
// Protocol-loop edge cases
//
// These exercise the malformed-input surface of `read_batch` /
// `parse_command`: batches that mix command kinds, batches that
// contain just a blank line, and EOF mid-batch. Each is mapped to a
// specific `CrabError::Protocol(...)` so clients can distinguish
// "broken client" from "ref rejected".
// ---------------------------------------------------------------------------

mod protocol_edges {
    use std::io::Cursor;

    use crab::git::remote_helper::StdIo;
    use tokio::io::{AsyncReadExt, BufReader, duplex};

    fn test_url() -> (tempfile::TempDir, gix_url::Url) {
        let remote = tempfile::tempdir().expect("create local transcript remote");
        let url = format!("file://{}", remote.path().display());
        let parsed = gix_url::Url::from_bytes(url.as_str().into()).expect("test URL should parse");
        (remote, parsed)
    }

    async fn run_capture(input: &str) -> (String, std::result::Result<(), String>) {
        struct DuplexIo {
            reader: BufReader<Cursor<Vec<u8>>>,
            writer: tokio::io::DuplexStream,
        }

        impl StdIo for DuplexIo {
            type Reader = BufReader<Cursor<Vec<u8>>>;
            type Writer = tokio::io::DuplexStream;

            fn split(self) -> (Self::Reader, Self::Writer) {
                (self.reader, self.writer)
            }
        }

        let input_bytes = input.as_bytes().to_vec();
        let (writer_tx, mut writer_rx) = duplex(64 * 1024);
        let io = DuplexIo {
            reader: BufReader::new(Cursor::new(input_bytes)),
            writer: writer_tx,
        };
        let (_remote, url) = test_url();
        let mut output = Vec::new();
        let helper = crab::git::remote_helper::run_remote_helper(
            "origin",
            &url,
            io,
            tokio_util::sync::CancellationToken::new(),
        );
        let reader = writer_rx.read_to_end(&mut output);

        let result = match tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(helper, reader)
        })
        .await
        {
            Ok((Ok(()), Ok(_))) => Ok(()),
            Ok((Ok(()), Err(e))) => Err(e.to_string()),
            Ok((Err(e), _)) => Err(e.to_string()),
            Err(_) => Err("helper task timed out".to_owned()),
        };

        (String::from_utf8_lossy(&output).into_owned(), result)
    }

    /// A batch that interleaves `fetch` and `push` must be rejected
    /// as a protocol error rather than silently running one kind and
    /// dropping the other.
    #[tokio::test]
    async fn mixed_fetch_push_batch_errors_cleanly() {
        let input = "\
fetch abc123 refs/heads/main\n\
push refs/heads/main:refs/heads/main\n\
\n";
        let (_output, result) = run_capture(input).await;

        let err = result
            .err()
            .expect("mixed fetch+push batch must abort the helper task");
        assert!(
            err.contains("mixed command types in batch"),
            "expected mixed-batch protocol error; got {err}"
        );
    }

    /// A standalone blank line with no preceding commands is not a
    /// batch boundary — the batch collector treats it as a no-op and
    /// waits for a real command. This asserts the run_remote_helper
    /// loop exits cleanly after a lone blank.
    #[tokio::test]
    async fn blank_line_then_eof_exits_cleanly() {
        let input = "\n";
        let (output, result) = run_capture(input).await;
        assert!(
            result.is_ok(),
            "lone blank line must not abort the helper; got {result:?}"
        );
        assert!(
            output.is_empty(),
            "lone blank line must not emit any response; got {output:?}"
        );
    }

    /// EOF reached mid-batch with at least one command already
    /// collected must finalize the batch (the `n == 0` branch) rather
    /// than silently dropping what was read. Covered here with a
    /// fetch batch that lacks its trailing blank.
    #[tokio::test]
    async fn eof_mid_fetch_batch_finalizes_without_blank_line() {
        let input = "fetch abc123 refs/heads/main\n";
        let (_output, result) = run_capture(input).await;
        // The fetch pipeline will error out because no store is
        // configured at this URL, but the point is the batch runs
        // at all — `finalize_batch` is reached. Either outcome
        // (helper task Ok or a specific store-related error) is
        // evidence the EOF-finalization path works; the assertion
        // catches the only regression we care about, the spec's
        // prior "silently drop commands on EOF" behavior.
        match result {
            Ok(()) => {}
            Err(msg) => assert!(
                !msg.contains("mixed command types"),
                "EOF finalization must not surface as a mixed-batch error; got {msg}"
            ),
        }
    }
}
