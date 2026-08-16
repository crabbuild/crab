//! Explicit contract tests for the released but unsupported protocol-v2
//! transport scaffold.

#![cfg(feature = "gix-transport")]

use std::io::{BufReader, Cursor};

use bstr::BString;
use gix_transport::client::blocking_io::Transport;
use gix_transport::client::{Error as TransportError, MessageKind, TransportWithoutIO, WriteMode};

use crab::git::fetch_transport::{
    StdioTransport, build_ref_advertisement_typed, parse_refspec_gix,
};
use crab::git::remote_helper::{ListOutput, RefEntry, format_capabilities, format_list_output};

fn transport() -> StdioTransport<BufReader<Cursor<Vec<u8>>>, Vec<u8>> {
    StdioTransport::new(
        BString::from("crab://bucket/repo"),
        BufReader::new(Cursor::new(Vec::new())),
        Vec::new(),
    )
}

#[test]
fn v2_transport_capabilities_are_not_advertised() {
    for has_commit_graph in [false, true] {
        let capabilities = format_capabilities(has_commit_graph);
        assert!(
            !capabilities
                .lines()
                .any(|line| { matches!(line, "connect" | "stateless-connect") })
        );
    }
}

#[test]
fn v2_handshake_fails_closed_before_reading_stdio() {
    let mut transport = transport();

    let result = transport.handshake(gix_transport::Service::UploadPack, &[]);

    assert!(matches!(
        result,
        Err(TransportError::AuthenticationUnsupported)
    ));
}

#[test]
fn v2_request_fails_closed_before_writing_stdio() {
    let mut transport = transport();

    let result = transport.request(WriteMode::Binary, MessageKind::Flush, false);

    assert!(matches!(
        result,
        Err(TransportError::AuthenticationUnsupported)
    ));
}

#[test]
fn v2_transport_reports_only_v2_with_stateless_request_lifetime() {
    let transport = transport();

    assert_eq!(
        transport.supported_protocol_versions(),
        &[gix_transport::Protocol::V2]
    );
    assert!(!transport.connection_persists_across_multiple_requests());
}

#[test]
fn typed_refs_and_gix_refspecs_match_the_v1_helper_contract() {
    use gix_hash::ObjectId;
    use gix_ref::{FullName, Reference, Target};

    let main_sha = "abc123def456abc123def456abc123def456abcd";
    let tag_sha = "111222333444555666777888999000aaabbbcccd";
    let main = Reference {
        name: FullName::try_from("refs/heads/main").expect("main ref name"),
        target: Target::Object(ObjectId::from_hex(main_sha.as_bytes()).expect("main oid")),
        peeled: None,
    };
    let tag = Reference {
        name: FullName::try_from("refs/tags/v1").expect("tag ref name"),
        target: Target::Object(ObjectId::from_hex(tag_sha.as_bytes()).expect("tag oid")),
        peeled: None,
    };
    let typed = build_ref_advertisement_typed([&main, &tag], Some("refs/heads/main"))
        .expect("typed advertisement");
    let canonical = format_list_output(&ListOutput {
        refs: vec![
            RefEntry {
                sha: main_sha.to_owned(),
                ref_name: "refs/heads/main".to_owned(),
                peeled: None,
            },
            RefEntry {
                sha: tag_sha.to_owned(),
                ref_name: "refs/tags/v1".to_owned(),
                peeled: None,
            },
        ],
        head_symref: Some("refs/heads/main".to_owned()),
    });

    assert_eq!(typed, canonical);
    assert!(
        !parse_refspec_gix("refs/heads/main:refs/heads/main")
            .expect("update refspec")
            .force
    );
    assert!(
        parse_refspec_gix("+refs/heads/main:refs/heads/main")
            .expect("force refspec")
            .force
    );
    let deletion = parse_refspec_gix(":refs/heads/old").expect("delete refspec");
    assert!(deletion.src.is_empty());
    assert_eq!(deletion.dst, "refs/heads/old");
}
