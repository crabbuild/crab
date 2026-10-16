use super::*;

#[tokio::test]
async fn spill_capacity_is_released_only_after_owned_file_removal() {
    let parent = tempfile::tempdir().unwrap();
    let space = ScratchSpace::create(parent.path(), 3).await.unwrap();
    let path = space.directory.path.clone();
    let file = space.write(Bytes::from_static(b"abc")).await.unwrap();
    assert_eq!(
        space.read(&file, 3).await.unwrap(),
        Bytes::from_static(b"abc")
    );
    assert!(space.write(Bytes::from_static(b"d")).await.is_err());
    space.remove(file).await.unwrap();
    space.write(Bytes::from_static(b"xyz")).await.unwrap();
    space.close().await.unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn spill_reads_reject_changed_bytes_lengths_limits_and_foreign_references() {
    for scenario in ["hash", "length", "limit", "foreign"] {
        let parent = tempfile::tempdir().unwrap();
        let space = ScratchSpace::create(parent.path(), 1024).await.unwrap();
        let other = ScratchSpace::create(parent.path(), 1024).await.unwrap();
        let file = space.write(Bytes::from_static(b"abc")).await.unwrap();
        let path = space.directory.file_path(&file).unwrap();
        let maximum = if scenario == "limit" { 2 } else { 1024 };
        match scenario {
            "hash" => std::fs::write(path, b"xyz").unwrap(),
            "length" => std::fs::write(path, b"abcd").unwrap(),
            _ => (),
        }
        let reader = if scenario == "foreign" {
            &other
        } else {
            &space
        };
        assert!(reader.read(&file, maximum).await.is_err(), "{scenario}");
        space.close().await.unwrap();
        other.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_retains_directory_until_detached_writer_exits() {
    for explicit_close in [false, true] {
        let parent = tempfile::tempdir().unwrap();
        let space = ScratchSpace::create(parent.path(), 1024).await.unwrap();
        let path = space.directory.path.clone();
        let started = Arc::new(tokio::sync::Notify::new());
        let (release, receiver) = std::sync::mpsc::channel();
        let (done, finished) = tokio::sync::oneshot::channel();
        {
            let started_job = started.clone();
            let future = space.blocking(move |directory| {
                started_job.notify_one();
                receiver.recv().map_err(worker)?;
                let result = std::fs::write(directory.path.join("late"), b"bytes").map_err(io);
                let _ = done.send(result);
                Ok(())
            });
            tokio::pin!(future);
            tokio::select! {
                _ = started.notified() => (),
                result = &mut future => panic!("blocked writer completed early: {result:?}"),
            }
        }
        if explicit_close {
            let mut close = tokio::spawn(space.close());
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(25), &mut close)
                    .await
                    .is_err()
            );
            release.send(()).unwrap();
            finished.await.unwrap().unwrap();
            close.await.unwrap().unwrap();
        } else {
            drop(space);
            assert!(path.exists());
            release.send(()).unwrap();
            finished.await.unwrap().unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while path.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        }
        assert!(!path.exists());
    }
}
