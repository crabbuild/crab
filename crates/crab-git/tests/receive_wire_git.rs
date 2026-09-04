use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use crab_git::{incoming_pack, receive_wire};
use gix_hash::ObjectId;

const NULL_CONFIG: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

struct GitChild(Child);
impl Drop for GitChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn git(directory: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = GitChild(
        Command::new("git")
            .current_dir(directory)
            .args([
                "-c",
                "user.name=Wire Test",
                "-c",
                "user.email=wire@example.invalid",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", NULL_CONFIG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    child.0.stdin.take().unwrap().write_all(input).unwrap();
    let mut output = String::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert!(child.0.wait().unwrap().success());
    output.trim().to_owned()
}

// send-pack wraps the HTTP request body in an extra pkt-line stream for its
// remote-curl peer. Unwrap only that transport envelope before calling Crab.
fn rpc_body(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut prefix = [0; 4];
        reader.read_exact(&mut prefix)?;
        let length = usize::from_str_radix(std::str::from_utf8(&prefix).unwrap(), 16).unwrap();
        if length == 0 {
            return Ok(body);
        }
        assert!((4..=65_520).contains(&length));
        assert!(body.len() + length < 1024 * 1024);
        let start = body.len();
        body.resize(start + length - 4, 0);
        reader.read_exact(&mut body[start..])?;
    }
}

#[test]
fn native_git_negotiates_exact_commands_packs_and_failure_status() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    git(path, &["init", "--bare", "--object-format=sha1", "."], b"");
    let tree = git(path, &["hash-object", "-w", "-t", "tree", "--stdin"], b"");
    let commit = git(path, &["commit-tree", &tree], b"wire fixture\n");
    let oid = ObjectId::from_hex(commit.as_bytes()).unwrap();
    git(path, &["update-ref", "refs/heads/main", &commit], b"");

    for (existing, refspecs, unpack, rejection, expected_objects) in [
        (
            false,
            vec!["main:refs/heads/main", "main:refs/tags/new"],
            None,
            None,
            Some(2),
        ),
        (true, vec![":refs/heads/main"], None, None, None),
        (true, vec!["main:refs/tags/new"], None, None, Some(0)),
        (
            false,
            vec!["main:refs/heads/main"],
            None,
            Some("write denied"),
            Some(2),
        ),
        (
            false,
            vec!["main:refs/heads/main"],
            Some("invalid pack"),
            None,
            Some(2),
        ),
    ] {
        let refs = if existing {
            BTreeMap::from([("refs/heads/main".to_owned(), oid)])
        } else {
            BTreeMap::new()
        };
        let mut child = GitChild(
            Command::new("git")
                .current_dir(path)
                .args([
                    "send-pack",
                    "--stateless-rpc",
                    "--atomic",
                    "--no-thin",
                    "fixture",
                ])
                .args(&refspecs)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", NULL_CONFIG)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut input = child.0.stdin.take().unwrap();
        receive_wire::advertise(&mut input, &refs).unwrap();
        input.flush().unwrap();
        let mut output = child.0.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let body = rpc_body(&mut output);
            let _ = sender.send((body, output));
        });
        let (body, _output) = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        let body = body.unwrap();
        reader.join().unwrap();
        let mut pack_bytes = body.as_slice();
        let request = receive_wire::read_request(&mut pack_bytes).unwrap();
        assert!(request.report_status);
        assert_eq!(request.updates.len(), refspecs.len());
        for update in &request.updates {
            assert_eq!(update.old, refs.get(&update.name).copied());
            assert_eq!(update.new, expected_objects.map(|_| oid));
        }
        if let Some(expected) = expected_objects {
            let pack = incoming_pack::quarantine(
                pack_bytes,
                path,
                incoming_pack::ReceiveLimits {
                    max_pack_bytes: 1024 * 1024,
                    max_objects: 10,
                    max_object_bytes: 1024 * 1024,
                    max_inflated_bytes: 1024 * 1024,
                    max_delta_depth: 10,
                },
                || false,
                |_| Ok(None),
            )
            .unwrap();
            assert_eq!(pack.received_objects(), expected);
            if expected != 0 {
                let object = pack.read_object(&oid).unwrap().unwrap();
                assert_eq!(object.kind, gix_object::Kind::Commit);
                assert!(object.data.ends_with(b"wire fixture\n"));
            }
        } else {
            assert!(
                pack_bytes.is_empty(),
                "deletion-only requests must not carry a pack"
            );
        }
        receive_wire::report(&mut input, &request.updates, unpack, rejection).unwrap();
        drop(input);
        let mut stderr = String::new();
        child
            .0
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert_eq!(
            child.0.wait().unwrap().success(),
            unpack.is_none() && rejection.is_none(),
            "{stderr}"
        );
        if let Some(reason) = rejection.or(unpack) {
            assert!(stderr.contains(reason), "{stderr}");
        }
    }
}
