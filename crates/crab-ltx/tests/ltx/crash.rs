use crab_ltx::{Db, Limits, LocalSegment, SegmentInfo, VerifiedPlan, restore_exact};
use std::io::{BufRead, Read, Write};
use std::process::{Command, Stdio};

// Test-harness entry point in a separate process. The parent kills it while the
// SQLite writer/read-lock connections are live; no orderly close is simulated.
#[test]
fn crash_writer() {
    let Some(path) = std::env::var_os("CRAB_LTX_CRASH_TEST_DIR") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let mut db = Db::open(&path.join("repo.sqlite"), Limits::default()).unwrap();
    db.transaction(|tx| {
        tx.execute("CREATE TABLE witnesses (value TEXT)", [])?;
        tx.execute("INSERT INTO witnesses VALUES ('survived process kill')", [])?;
        Ok(())
    })
    .unwrap();
    let batch = db.capture().unwrap();
    assert_eq!(batch.segments.len(), 1);
    let segment = &batch.segments[0];
    std::fs::copy(segment.path(), path.join("captured.ltx")).unwrap();
    let info = segment.info();
    let digest: String = info
        .blake3
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!(
        "LTX-CUT {} {} {} {} {} {} {} {}",
        info.min_txid,
        info.max_txid,
        info.page_size,
        info.database_pages,
        info.pre_checksum,
        info.post_checksum,
        info.size_bytes,
        digest
    );
    std::io::stdout().flush().unwrap();
    let mut byte = [0];
    std::io::stdin().read_exact(&mut byte).unwrap();
    panic!("parent must kill this process, not release stdin");
}

#[test]
fn captured_sql_survives_process_kill_and_source_directory_loss() {
    let source = tempfile::TempDir::new().unwrap();
    let replica = tempfile::TempDir::new().unwrap();
    let restored = tempfile::TempDir::new().unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["crash_writer", "--nocapture"])
        .env("CRAB_LTX_CRASH_TEST_DIR", source.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut child = KillOnDrop(child);
    // Child::wait closes its stored stdin before reaping. Keep the pipe alive
    // independently so an EOF panic cannot race the intended process kill.
    let _stdin = child.0.stdin.take().unwrap();
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if let Some(cut) = line.strip_prefix("LTX-CUT ") {
                let _ = send.send(cut.to_owned());
                break;
            }
        }
    });
    let cut = receive
        .recv_timeout(std::time::Duration::from_secs(15))
        .unwrap();
    let fields: Vec<_> = cut.split_whitespace().collect();
    let mut digest = [0u8; 32];
    for (i, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&fields[7][i * 2..i * 2 + 2], 16).unwrap();
    }
    let info = SegmentInfo {
        min_txid: fields[0].parse().unwrap(),
        max_txid: fields[1].parse().unwrap(),
        page_size: fields[2].parse().unwrap(),
        database_pages: fields[3].parse().unwrap(),
        pre_checksum: fields[4].parse().unwrap(),
        post_checksum: fields[5].parse().unwrap(),
        size_bytes: fields[6].parse().unwrap(),
        blake3: digest,
    };
    child.0.kill().unwrap();
    let exit = child.0.wait().unwrap();
    assert!(!exit.success());
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(exit.signal(), Some(9));
    }
    let path = replica.path().join("captured.ltx");
    std::fs::copy(source.path().join("captured.ltx"), &path).unwrap();
    source.close().unwrap();
    let position = info.position();
    let plan = VerifiedPlan::new(
        &[LocalSegment::new(path, info)],
        position,
        Limits::default(),
    )
    .unwrap();
    let destination = restored.path().join("repo.sqlite");
    restore_exact(&plan, &destination).unwrap();
    let conn = crab_ltx::rusqlite::Connection::open(destination).unwrap();
    let value: String = conn
        .query_row("SELECT value FROM witnesses", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "survived process kill");
}

struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
