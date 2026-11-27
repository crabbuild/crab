use super::*;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str], input: &[u8], protocol: bool) -> Vec<u8> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env("GIT_AUTHOR_NAME", "Crab")
        .env("GIT_AUTHOR_EMAIL", "crab@example.invalid")
        .env("GIT_COMMITTER_NAME", "Crab")
        .env("GIT_COMMITTER_EMAIL", "crab@example.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if protocol {
        command.env("GIT_PROTOCOL", "version=2");
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn repository(dir: &Path) {
    git(dir, &["init", "-q", "-b", "trunk"], b"", false);
    for revision in 0..3 {
        std::fs::write(dir.join("file"), format!("{revision}\n")).unwrap();
        git(dir, &["add", "file"], b"", false);
        git(dir, &["commit", "-qm", "commit"], b"", false);
    }
    git(dir, &["tag", "-a", "-m", "release", "v1"], b"", false);
    git(
        dir,
        &["update-ref", "refs/pull/1/head", "HEAD~1"],
        b"",
        false,
    );
}

fn upload_pack(dir: &Path, request: &[u8]) -> Vec<u8> {
    git(dir, &["upload-pack", "--stateless-rpc", "."], request, true)
}

#[test]
fn native_upload_pack_advertises_lists_and_streams_a_complete_pack() {
    let temp = tempfile::tempdir().unwrap();
    repository(temp.path());
    let advertisement = git(
        temp.path(),
        &["upload-pack", "--stateless-rpc", "--advertise-refs", "."],
        b"",
        true,
    );
    let mut announced = b"001e# service=git-upload-pack\n0000".to_vec();
    announced.extend_from_slice(&advertisement);
    for body in [&advertisement, &announced] {
        let capabilities = parse_advertisement(body).unwrap();
        assert!(capabilities.ls_refs.is_some());
        assert!(capabilities.fetch.is_some());
    }

    let refs = parse_ls_refs(&upload_pack(
        temp.path(),
        &encode_ls_refs("crab/test", &["HEAD", "refs/heads/", "refs/tags/"]),
    ))
    .unwrap();
    let head = refs
        .iter()
        .find(|reference| reference.name == "HEAD")
        .unwrap();
    assert_eq!(head.symref_target.as_deref(), Some("refs/heads/trunk"));
    let tag = refs
        .iter()
        .find(|reference| reference.name == "refs/tags/v1")
        .unwrap();
    assert!(tag.peeled.is_some());
    assert!(
        !refs
            .iter()
            .any(|reference| reference.name.starts_with("refs/pull/"))
    );

    let wants = refs
        .iter()
        .filter(|reference| reference.name != "HEAD")
        .filter_map(|reference| reference.oid)
        .collect::<Vec<_>>();
    let response = upload_pack(temp.path(), &encode_fetch("crab/test", &wants, &[]));
    let mut decoder = FetchResponse::new();
    let mut pack = Vec::new();
    for part in response.chunks(7) {
        decoder
            .feed(part, &mut |bytes: &[u8]| {
                pack.extend_from_slice(bytes);
                Ok::<(), std::convert::Infallible>(())
            })
            .unwrap();
    }
    decoder.finish().unwrap();
    let mut scanner = crate::pack_stream::PackScanner::new(crate::pack_stream::StreamLimits {
        max_pack_bytes: 1 << 30,
        max_objects: 1000,
        max_object_bytes: 1 << 20,
        max_inflated_bytes: 1 << 30,
        max_delta_depth: 64,
        max_cache_bytes: 0,
    });
    scanner.feed(&pack).unwrap();
    let table = scanner.finish().unwrap();
    // Three commits, three trees, three blobs and one annotated tag.
    assert_eq!(table.entries().len(), 10);
}

fn response(lines: &[&[u8]]) -> Vec<u8> {
    let mut body = Vec::new();
    for line in lines {
        match *line {
            b"0000" | b"0001" => body.extend_from_slice(line),
            line => {
                body.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
                body.extend_from_slice(line);
            }
        }
    }
    body
}

fn decode(body: &[u8]) -> std::result::Result<Vec<u8>, FeedError<std::convert::Infallible>> {
    let mut decoder = FetchResponse::new();
    let mut pack = Vec::new();
    decoder.feed(body, &mut |bytes: &[u8]| {
        pack.extend_from_slice(bytes);
        Ok(())
    })?;
    decoder.finish().map_err(FeedError::Wire)?;
    Ok(pack)
}

#[test]
fn side_band_errors_shallow_sources_and_truncation_fail_closed() {
    let pack = decode(&response(&[
        b"packfile\n",
        b"\x01PACK",
        b"\x02progress\r",
        b"0000",
    ]))
    .unwrap();
    assert_eq!(pack, b"PACK");
    assert!(matches!(
        decode(&response(&[b"packfile\n", b"\x03access denied", b"0000"])),
        Err(FeedError::Wire(FetchWireError::Remote(message))) if message == "access denied"
    ));
    assert!(matches!(
        decode(&response(&[
            b"shallow-info\n",
            b"shallow 0000000000000000000000000000000000000001\n"
        ])),
        Err(FeedError::Wire(FetchWireError::Shallow))
    ));
    assert!(matches!(
        decode(&response(&[b"packfile\n", b"\x01PACK"])),
        Err(FeedError::Wire(FetchWireError::Malformed(_)))
    ));
    assert!(matches!(
        parse_advertisement(&response(&[b"version 1\n", b"0000"])),
        Err(FetchWireError::VersionTwoRequired)
    ));
}
