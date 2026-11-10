use std::io;
use std::time::Duration;

use tokio::process::Child;
use tokio_util::sync::CancellationToken;

use super::Exit;

pub(super) const GRACE: Duration = Duration::from_secs(5);

pub(super) async fn supervise(
    child: &mut Child,
    pid: u32,
    stop: &CancellationToken,
    force: &CancellationToken,
) -> io::Result<Exit> {
    let status = tokio::select! {
        status = child.wait() => status?,
        _ = stop.cancelled() => {
            if force.is_cancelled() {
                signal_group(pid, libc::SIGKILL).await?;
                child.wait().await?
            } else {
                signal_group(pid, libc::SIGTERM).await?;
                tokio::select! {
                    status = child.wait() => status?,
                    _ = tokio::time::sleep(GRACE) => { signal_group(pid, libc::SIGKILL).await?; child.wait().await? },
                    _ = force.cancelled() => { signal_group(pid, libc::SIGKILL).await?; child.wait().await? },
                }
            }
        }
    };
    // The leader can exit while descendants retain its pipes. Its owner must
    // retire that group before reporting closure or releasing workspace writers.
    signal_group(pid, libc::SIGTERM).await?;
    let until = tokio::time::Instant::now() + GRACE;
    while group_exists(pid).await? {
        if force.is_cancelled() || tokio::time::Instant::now() >= until {
            signal_group(pid, libc::SIGKILL).await?;
        }
        if tokio::time::Instant::now() >= until + GRACE {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Native process group closure could not be confirmed",
            ));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    };
    Ok(Exit {
        code: status.code(),
        signal,
    })
}

async fn signal_group(pid: u32, signal: i32) -> io::Result<bool> {
    let group = i32::try_from(pid).map_err(io::Error::other)?;
    let until = tokio::time::Instant::now() + GRACE;
    loop {
        // SAFETY: the PID comes from our freshly spawned process-group leader;
        // the caller owns it until group closure and supplies a valid POSIX signal.
        if unsafe { libc::kill(-group, signal) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        // XNU filters zombies before checking group signal permissions, so a
        // group awaiting reaping returns EPERM. Require ESRCH after a bounded
        // wait; a real permission failure must never become confirmed closure.
        if cfg!(target_os = "macos")
            && error.raw_os_error() == Some(libc::EPERM)
            && tokio::time::Instant::now() < until
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        }
        return Err(error);
    }
}

pub(super) async fn group_exists(pid: u32) -> io::Result<bool> {
    signal_group(pid, 0).await
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[tokio::test(flavor = "multi_thread")]
    async fn zombie_only_group_requires_reaping_before_confirming_closure() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        tokio::time::timeout(GRACE, async {
            loop {
                // SAFETY: query only the process group created by this test.
                let result = unsafe { libc::kill(-(pid as i32), 0) };
                if result == -1 {
                    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let reaper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            child.wait().unwrap();
        });
        assert!(!signal_group(pid, libc::SIGTERM).await.unwrap());
        reaper.await.unwrap();
    }
}
