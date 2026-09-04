use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
};

use crab_git::receive_wire::{self, ReceiveWireError};
use gix_hash::ObjectId;
use gix_packetline::blocking_io::encode;

fn packet(line: &[u8], out: &mut Vec<u8>) {
    encode::data_to_write(line, out).unwrap();
}

fn command(old: &str, new: &str, name: &str, capabilities: Option<&str>) -> Vec<u8> {
    let mut line = format!("{old} {new} {name}");
    if let Some(capabilities) = capabilities {
        line.push('\0');
        line.push_str(capabilities);
    }
    line.into_bytes()
}

#[test]
fn fragmented_commands_preserve_exact_ids_order_and_pack_boundary() {
    struct Fragmented<R>(R);
    impl<R: Read> Read for Fragmented<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let len = buf.len().min(1);
            self.0.read(&mut buf[..len])
        }
    }
    let mut body = Vec::new();
    let zero = "0".repeat(40);
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    packet(
        &command(
            &zero,
            &a,
            "refs/heads/new",
            Some("report-status atomic ofs-delta agent=git/2.45 object-format=sha1"),
        ),
        &mut body,
    );
    packet(&command(&a, &b, "refs/heads/main", None), &mut body);
    packet(&command(&b, &zero, "refs/tags/old", None), &mut body);
    encode::flush_to_write(&mut body).unwrap();
    body.extend_from_slice(b"PACK untouched bytes");
    let mut reader = Fragmented(Cursor::new(body));
    let request = receive_wire::read_request(&mut reader).unwrap();
    assert!(request.report_status);
    let actual: Vec<_> = request
        .updates
        .into_iter()
        .map(|u| {
            (
                u.name,
                u.old.map(|id| id.to_string()),
                u.new.map(|id| id.to_string()),
            )
        })
        .collect();
    assert_eq!(
        actual,
        vec![
            ("refs/heads/new".into(), None, Some(a.clone())),
            ("refs/heads/main".into(), Some(a), Some(b.clone())),
            ("refs/tags/old".into(), Some(b), None)
        ]
    );
    let mut tail = Vec::new();
    reader.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, b"PACK untouched bytes");
}

#[test]
fn malformed_or_unadvertised_commands_are_rejected_before_pack_reads() {
    let zero = "0".repeat(40);
    let a = "a".repeat(40);
    for line in [
        command(&zero, &a, "refs/heads/new", None),
        command(
            &zero,
            &a,
            "refs/heads/new",
            Some("report-status report-status"),
        ),
        command(&zero, &a, "refs/heads/new", Some("push-options")),
        command(&zero, &a, "refs/heads/new", Some("object-format=sha256")),
        command(&zero, &a, "refs/heads/new", Some("report-status\natomic")),
        command(&zero, &a, "refs/heads/new", Some("agent=\0bad")),
        command(&zero, &a, "HEAD", Some("report-status")),
        command(&zero, &a, "refs/heads/../bad", Some("report-status")),
        command(&zero, &a, "refs/heads/new extra", Some("report-status")),
        command(&zero, &zero, "refs/heads/new", Some("report-status")),
        command(&a, &a, "refs/heads/new", Some("report-status")),
        command("abcd", &a, "refs/heads/new", Some("report-status")),
        command(&"g".repeat(40), &a, "refs/heads/new", Some("report-status")),
        format!("shallow {a}\n").into_bytes(),
        b"push-cert\0report-status".to_vec(),
    ] {
        let mut body = Vec::new();
        packet(&line, &mut body);
        body.extend_from_slice(b"0000PACK");
        let mut reader = Cursor::new(body);
        assert!(receive_wire::read_request(&mut reader).is_err(), "{line:?}");
        assert!(reader.position() as usize <= reader.get_ref().len() - 8);
    }
}

#[test]
fn duplicate_destinations_and_repeated_capability_sections_are_rejected() {
    for (name, capabilities) in [
        ("refs/heads/main", None),
        ("refs/heads/other", Some("report-status")),
    ] {
        let mut body = Vec::new();
        packet(
            &command(
                &"0".repeat(40),
                &"a".repeat(40),
                "refs/heads/main",
                Some("report-status"),
            ),
            &mut body,
        );
        packet(
            &command(&"0".repeat(40), &"b".repeat(40), name, capabilities),
            &mut body,
        );
        body.extend_from_slice(b"0000");
        assert!(receive_wire::read_request(&mut body.as_slice()).is_err());
    }
}

#[test]
fn packet_and_aggregate_limits_stop_unbounded_command_sections() {
    for body in [
        b"0001".as_slice(),
        b"0002",
        b"0003",
        b"0004",
        b"ffff",
        b"xyz1",
        b"0010short",
        b"",
    ] {
        assert!(receive_wire::read_request(&mut Cursor::new(body)).is_err());
    }
    for long_names in [false, true] {
        let mut body = Vec::new();
        for i in 0..=receive_wire::MAX_COMMANDS {
            let suffix = if long_names {
                "x".repeat(2048)
            } else {
                String::new()
            };
            let name = format!("refs/heads/branch-{i}{suffix}");
            packet(
                &command(
                    &"0".repeat(40),
                    &"a".repeat(40),
                    &name,
                    (i == 0).then_some("report-status"),
                ),
                &mut body,
            );
        }
        body.extend_from_slice(b"0000PACK");
        let mut reader = Cursor::new(body);
        assert!(matches!(
            receive_wire::read_request(&mut reader),
            Err(ReceiveWireError::Protocol(
                "receive command section exceeds its limit"
            ))
        ));
        assert!(reader.position() as usize <= receive_wire::MAX_COMMAND_BYTES + 4);
    }
}

#[test]
fn advertisement_and_status_are_native_packet_lines() {
    let mut empty = Vec::new();
    receive_wire::advertise(&mut empty, &BTreeMap::new()).unwrap();
    assert!(
        empty
            .windows(b" capabilities^{}\0report-status".len())
            .any(|s| s == b" capabilities^{}\0report-status")
    );
    assert!(empty.ends_with(b"0000"));
    let refs = BTreeMap::from([("refs/heads/main".into(), ObjectId::from([0xab; 20]))]);
    let mut advertisement = Vec::new();
    receive_wire::advertise(&mut advertisement, &refs).unwrap();
    assert!(
        advertisement
            .windows(b"refs/heads/main\0report-status".len())
            .any(|s| s == b"refs/heads/main\0report-status")
    );
    let updates = vec![crab_git::receive_plan::RefUpdate {
        name: "refs/heads/main".into(),
        old: None,
        new: Some(ObjectId::from([0xab; 20])),
    }];
    for (unpack, reject, expected) in [
        (None, None, "ok refs/heads/main\n"),
        (None, Some("stale info"), "ng refs/heads/main stale info\n"),
        (
            Some("invalid pack"),
            None,
            "ng refs/heads/main invalid pack\n",
        ),
    ] {
        let mut report = Vec::new();
        receive_wire::report(&mut report, &updates, unpack, reject).unwrap();
        let mut oracle = Vec::new();
        packet(
            format!("unpack {}\n", unpack.unwrap_or("ok")).as_bytes(),
            &mut oracle,
        );
        packet(expected.as_bytes(), &mut oracle);
        encode::flush_to_write(&mut oracle).unwrap();
        assert_eq!(report, oracle);
    }
}

#[test]
fn lone_flush_and_optional_trailing_lf_follow_native_framing() {
    let mut probe = b"0000PACK".as_slice();
    assert!(
        receive_wire::read_request(&mut probe)
            .unwrap()
            .updates
            .is_empty()
    );
    assert_eq!(probe, b"PACK");
    let mut body = Vec::new();
    let mut line = command(
        &"0".repeat(40),
        &"a".repeat(40),
        "refs/heads/main",
        Some("report-status"),
    );
    line.push(b'\n');
    packet(&line, &mut body);
    body.extend_from_slice(b"0000");
    assert_eq!(
        receive_wire::read_request(&mut body.as_slice())
            .unwrap()
            .updates
            .len(),
        1
    );
}

#[test]
fn invalid_output_is_rejected_before_writing_partial_status() {
    for name in [
        "HEAD".to_owned(),
        "refs/heads/ok\nok refs/heads/other".to_owned(),
        format!("refs/heads/{}", "a".repeat(65_516)),
    ] {
        let mut output = Vec::new();
        let refs = BTreeMap::from([(name.clone(), ObjectId::from([0xab; 20]))]);
        assert!(receive_wire::advertise(&mut output, &refs).is_err());
        assert!(output.is_empty());
        let updates = [crab_git::receive_plan::RefUpdate {
            name,
            old: None,
            new: Some(ObjectId::from([0xab; 20])),
        }];
        assert!(receive_wire::report(&mut output, &updates, None, None).is_err());
        assert!(output.is_empty());
    }
    for message in ["", "ok", "bad\nline", "bad\0line"] {
        let mut output = Vec::new();
        assert!(receive_wire::report(&mut output, &[], None, Some(message)).is_err());
        assert!(output.is_empty());
    }
}
