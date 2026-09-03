use super::*;

#[cfg(target_os = "macos")]
#[test]
fn cleanup_accepts_a_group_exiting_before_waitid_can_report_it() {
    // A short producer can leave killpg's recipient set before waitid sees
    // its exit. Repeat the real pipe/exit boundary without delaying cleanup.
    for _ in 0..32 {
        let mut process = Command::new("/usr/bin/head");
        process
            .args(["-c", "65536", "/dev/zero"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut process = CommandWrap::from(process);
        process.wrap(process_wrap::std::ProcessGroup::leader());
        let mut child = OwnedChild {
            inner: process.spawn().unwrap(),
            finished: false,
        };
        let mut stdout = child.inner.stdout().take().unwrap();
        stdout.read_exact(&mut [0; 65536]).unwrap();
        child.stop().unwrap();
    }
}

#[cfg(target_os = "macos")]
#[test]
fn cleanup_does_not_hide_permission_errors_for_a_live_group() {
    let mut process = Command::new("/bin/sleep");
    process.arg("30");
    let mut process = CommandWrap::from(process);
    process.wrap(process_wrap::std::ProcessGroup::leader());
    let mut child = OwnedChild {
        inner: process.spawn().unwrap(),
        finished: false,
    };
    let result = child.signal_result(Err(io::Error::from_raw_os_error(libc::EPERM)));
    child.stop().unwrap();
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EPERM));
}
