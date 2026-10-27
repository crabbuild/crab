use super::*;

#[test]
fn protocol_error_payload_uses_err_framing_and_sanitizes_controls() {
    assert_eq!(
        protocol_error_payload("request\nfailed\tcleanly"),
        b"ERR request failed cleanly\n"
    );
}

#[test]
fn protocol_error_payload_respects_the_packet_line_bound() {
    let payload = protocol_error_payload(&"x".repeat(MAX_PACKET_BYTES));

    assert_eq!(payload.len() + 4, MAX_PACKET_BYTES);
    assert!(payload.starts_with(b"ERR "));
    assert!(payload.ends_with(b"\n"));
}

#[tokio::test]
async fn rejected_request_writes_one_terminal_err_packet() {
    let mut writer = Vec::new();
    let cancellation = CancellationToken::new();
    let error = protocol("request rejected");
    let expected = packet(&protocol_error_payload(&error.to_string()));

    reject_protocol_request(&mut writer, error, &cancellation)
        .await
        .expect_err("the semantic request error must remain terminal");

    assert_eq!(writer, expected);
}

#[test]
fn combines_repeated_fetch_filters() {
    let request = parse_fetch(&[
        "want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        "filter blob:none".to_owned(),
        "filter tree:1".to_owned(),
    ])
    .expect("repeated filters should use intersection semantics");
    assert_eq!(request.filter.canonical_spec(), "combine:blob:none+tree:1");
}

#[test]
fn accepts_supported_filter_grammar_before_planning() {
    for filter in [
        "blob:limit=1m",
        "tree:1",
        "object:type=blob",
        "sparse:oid=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "combine:blob%3Anone+tree%3A1",
    ] {
        parse_fetch(&[
            format!("want {}", "a".repeat(40)),
            format!("filter {filter}"),
        ])
        .expect("supported filters must parse before planning");
    }
}

#[test]
fn rejects_unsupported_filter_before_planning() {
    let error = parse_fetch(&[
        format!("want {}", "a".repeat(40)),
        "filter blob:depth=1".to_owned(),
    ])
    .expect_err("unsupported filters must fail in the wire parser");
    assert!(error.to_string().contains("unsupported filter"));
}

#[test]
fn rejects_unadvertised_fetch_arguments_before_planning() {
    for argument in [
        "want-ref refs/heads/main",
        "packfile-uris https",
        "wait-for-done",
        "server-option trace=1",
    ] {
        let error = parse_fetch(&[format!("want {}", "a".repeat(40)), argument.to_owned()])
            .expect_err("unadvertised fetch arguments must fail in the wire parser");
        assert!(
            error.to_string().contains("unsupported fetch argument"),
            "{argument} unexpectedly produced {error}"
        );
    }
}

#[test]
fn parses_ref_prefix_arguments_with_inline_values() {
    let request = parse_ls_refs(&[
        "symrefs".to_owned(),
        "ref-prefix refs/heads/".to_owned(),
        "ref-prefix refs/tags/".to_owned(),
    ])
    .expect("inline ref-prefix values should parse");
    assert!(request.symrefs);
    assert_eq!(request.prefixes, ["refs/heads/", "refs/tags/"]);
}

#[tokio::test]
async fn command_request_requires_capability_delimiter() {
    let mut bytes = packet(b"command=ls-refs\n");
    bytes.extend_from_slice(b"0000");
    let mut reader = BufReader::new(Cursor::new(bytes));
    let cancellation = CancellationToken::new();

    let error = read_command_request(&mut reader, &cancellation)
        .await
        .expect_err("missing delimiter must be rejected");
    assert!(error.to_string().contains("missing its delimiter"));
}

#[tokio::test]
async fn closed_v2_session_is_not_an_early_eof_error() {
    let mut reader = BufReader::new(Cursor::new(Vec::<u8>::new()));
    let cancellation = CancellationToken::new();
    assert!(
        read_command_request(&mut reader, &cancellation)
            .await
            .expect("clean close should be accepted")
            .is_none()
    );
}

#[tokio::test]
async fn command_request_keeps_pkt_line_bytes_after_terminal_request() {
    let mut bytes = packet(b"command=ls-refs\n");
    bytes.extend_from_slice(b"0001");
    bytes.extend_from_slice(&packet(b"symrefs\n"));
    bytes.extend_from_slice(b"0000tail");
    let mut reader = BufReader::new(Cursor::new(bytes));
    let cancellation = CancellationToken::new();

    let request = read_command_request(&mut reader, &cancellation)
        .await
        .expect("request should parse")
        .expect("request should be present");
    assert_eq!(request.command, "ls-refs");
    assert!(request.capabilities.is_empty());
    assert_eq!(request.args, ["symrefs"]);

    let mut tail = Vec::new();
    reader
        .read_to_end(&mut tail)
        .await
        .expect("tail should remain readable");
    assert_eq!(tail, b"tail");
}

#[tokio::test]
async fn command_request_accepts_large_promisor_want_batch() {
    let mut bytes = packet(b"command=fetch\n");
    bytes.extend_from_slice(b"0001");
    for index in 0..10_000 {
        bytes.extend_from_slice(&packet(format!("want {index:040x}\n").as_bytes()));
    }
    bytes.extend_from_slice(&packet(b"filter blob:none\n"));
    bytes.extend_from_slice(&packet(b"done\n"));
    bytes.extend_from_slice(b"0000");
    let mut reader = BufReader::new(Cursor::new(bytes));
    let cancellation = CancellationToken::new();

    let request = read_command_request(&mut reader, &cancellation)
        .await
        .expect("large promisor request should stay within the byte bound")
        .expect("request should be present");
    assert_eq!(request.args.len(), 10_002);
}

#[tokio::test]
async fn request_byte_limit_is_enforced_before_flush() {
    let mut bytes = packet(b"command=ls-refs\n");
    bytes.extend_from_slice(b"0001");
    let oversized = vec![b'a'; MAX_PACKET_BYTES - 4];
    for _ in 0..65 {
        bytes.extend_from_slice(&packet(&oversized));
    }
    bytes.extend_from_slice(b"0000");
    let mut reader = BufReader::new(Cursor::new(bytes));
    let cancellation = CancellationToken::new();

    let error = read_command_request(&mut reader, &cancellation)
        .await
        .expect_err("oversized requests must fail before the flush packet");
    assert!(error.to_string().contains("exceeds bounds"));
}

#[test]
fn fetch_done_must_be_the_final_argument() {
    let error = parse_fetch(&[
        format!("want {}", "a".repeat(40)),
        "done".to_owned(),
        "no-progress".to_owned(),
    ])
    .expect_err("arguments after done must be rejected");
    assert!(error.to_string().contains("follow done"));
}

#[test]
fn relative_deepen_requires_a_depth() {
    let error = parse_fetch(&[
        format!("want {}", "a".repeat(40)),
        "deepen-relative".to_owned(),
    ])
    .expect_err("relative deepen without depth must be rejected");
    assert!(error.to_string().contains("requires deepen"));
}

#[test]
fn invalid_hidden_ref_glob_fails_closed() {
    let error = compile_hidden_refs(&["[".to_owned()])
        .expect_err("invalid hidden-ref patterns must reject the session");
    assert!(
        error
            .to_string()
            .contains("invalid transfer.hideRefs pattern")
    );
}

#[test]
fn reachable_non_tip_want_is_denied_by_default() {
    let ancestor = ObjectId::from_hex("a".repeat(40).as_bytes()).expect("ancestor oid");
    let tip = ObjectId::from_hex("b".repeat(40).as_bytes()).expect("tip oid");
    let request = FetchRequest {
        wants: vec![ancestor],
        ..FetchRequest::default()
    };
    let visible_refs = vec!["refs/heads/main".to_owned()];
    let visibility = GitVisibilityIndex::new(
        1,
        "c".repeat(64),
        "d".repeat(64),
        std::collections::BTreeMap::from([(
            visible_refs[0].clone(),
            vec![ancestor.to_string(), tip.to_string()],
        )]),
    )
    .expect("valid visibility proof");

    let error = validate_fetch_wants(
        &HashSet::from([tip]),
        &visibility,
        &visible_refs,
        &request,
        &FetchAdmissionPolicy::default(),
    )
    .expect_err("a reachable non-tip want must be denied without opt-in");

    assert!(error.to_string().contains("denied by upload-pack policy"));
}

#[test]
fn filtered_reachable_non_tip_want_is_accepted_by_default() {
    let blob = ObjectId::from_hex("a".repeat(40).as_bytes()).expect("blob oid");
    let tip = ObjectId::from_hex("b".repeat(40).as_bytes()).expect("tip oid");
    let request = FetchRequest {
        wants: vec![blob],
        filter: UploadPackFilter::BlobNone,
        ..FetchRequest::default()
    };
    let visible_refs = vec!["refs/heads/main".to_owned()];
    let visibility = GitVisibilityIndex::new(
        1,
        "c".repeat(64),
        "d".repeat(64),
        std::collections::BTreeMap::from([(
            visible_refs[0].clone(),
            vec![blob.to_string(), tip.to_string()],
        )]),
    )
    .expect("valid visibility proof");

    let result = validate_fetch_wants(
        &HashSet::from([tip]),
        &visibility,
        &visible_refs,
        &request,
        &FetchAdmissionPolicy::default(),
    );

    assert!(result.is_ok());
}

#[test]
fn filtered_reachable_non_tip_want_outside_visible_ref_closure_is_denied() {
    let hidden_blob = ObjectId::from_hex("a".repeat(40).as_bytes()).expect("hidden blob oid");
    let visible_blob = ObjectId::from_hex("b".repeat(40).as_bytes()).expect("visible blob oid");
    let tip = ObjectId::from_hex("c".repeat(40).as_bytes()).expect("tip oid");
    let request = FetchRequest {
        wants: vec![hidden_blob],
        filter: UploadPackFilter::BlobNone,
        ..FetchRequest::default()
    };
    let visible_refs = vec!["refs/heads/main".to_owned()];
    let visibility = GitVisibilityIndex::new(
        1,
        "d".repeat(64),
        "e".repeat(64),
        std::collections::BTreeMap::from([(
            visible_refs[0].clone(),
            vec![visible_blob.to_string(), tip.to_string()],
        )]),
    )
    .expect("valid visibility proof");

    let error = validate_fetch_wants(
        &HashSet::from([tip]),
        &visibility,
        &visible_refs,
        &request,
        &FetchAdmissionPolicy::default(),
    )
    .expect_err("objects outside visible ref closure must remain denied");

    assert!(error.to_string().contains("denied by upload-pack policy"));
}

#[test]
fn reachable_non_tip_want_is_accepted_when_enabled() {
    let ancestor = ObjectId::from_hex("a".repeat(40).as_bytes()).expect("ancestor oid");
    let tip = ObjectId::from_hex("b".repeat(40).as_bytes()).expect("tip oid");
    let request = FetchRequest {
        wants: vec![ancestor],
        ..FetchRequest::default()
    };
    let visible_refs = vec!["refs/heads/main".to_owned()];
    let visibility = GitVisibilityIndex::new(
        1,
        "c".repeat(64),
        "d".repeat(64),
        std::collections::BTreeMap::from([(
            visible_refs[0].clone(),
            vec![ancestor.to_string(), tip.to_string()],
        )]),
    )
    .expect("valid visibility proof");
    let policy = FetchAdmissionPolicy {
        allow_reachable_sha_in_want: true,
        ..FetchAdmissionPolicy::default()
    };

    let result = validate_fetch_wants(
        &HashSet::from([tip]),
        &visibility,
        &visible_refs,
        &request,
        &policy,
    );

    assert!(result.is_ok());
}

#[tokio::test]
async fn missing_capsule_payload_is_reported_after_discovery() {
    let (layout, _, tip) = Box::pin(capsule_negotiation::fixture(false)).await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    remove_pinned_runs(&layout, &root).await;
    let mut request = packet(b"command=ls-refs\n");
    request.extend_from_slice(b"00010000");
    request.extend(packet(b"command=fetch\n"));
    request.extend_from_slice(b"0001");
    request.extend(packet(format!("want {tip}\n").as_bytes()));
    request.extend(packet(b"done\n"));
    request.extend_from_slice(b"0000");
    let mut input = BufReader::new(Cursor::new(request));
    let mut output = Vec::new();
    let runtime = Arc::new(RemoteGitRuntime::default());
    let result = Box::pin(serve(
        &mut input,
        &mut output,
        layout.store(),
        layout.repo_prefix(),
        &[],
        &FetchAdmissionPolicy::default(),
        false,
        root,
        &runtime,
        &CancellationToken::new(),
    ))
    .await;
    runtime.shutdown().await;
    let error = result.unwrap_err();
    assert_eq!(output[0], b'\n');
    let cancellation = CancellationToken::new();
    let mut input = BufReader::new(Cursor::new(&output[1..]));
    while read_packet(&mut input, &cancellation).await.unwrap() != Packet::Flush {}
    let mut advertised = Vec::new();
    loop {
        match read_packet(&mut input, &cancellation).await.unwrap() {
            Packet::ResponseEnd => break,
            Packet::Data(bytes) => advertised.extend(bytes),
            Packet::Flush => {}
            other => panic!("unexpected discovery packet: {other:?}"),
        }
    }
    assert!(
        String::from_utf8(advertised)
            .unwrap()
            .contains(&tip.to_string())
    );
    assert_eq!(
        read_packet(&mut input, &cancellation).await.unwrap(),
        Packet::Data(protocol_error_payload(&error.to_string()))
    );
    assert!(
        !output
            .windows(b"packfile\n".len())
            .any(|bytes| bytes == b"packfile\n")
    );
}
