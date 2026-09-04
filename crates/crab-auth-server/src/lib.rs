pub mod doctor;
pub mod error;
pub mod output;
pub mod receive;
pub mod view;

#[cfg(test)]
mod test_support {
    pub(crate) fn unavailable_workspace_child(test_name: &str) -> bool {
        const CHILD_WORKSPACE: &str = "CRAB_TEST_UNAVAILABLE_WORKSPACE";
        if let Some(path) = std::env::var_os(CHILD_WORKSPACE) {
            let path = std::path::Path::new(&path);
            assert!(path.is_file(), "workspace fixture must be a regular file");
            tempfile::env::override_temp_dir(path).expect("isolated child override");
            return true;
        }

        // The temporary-directory override is process-global. Run exactly one
        // child test so parallel tests and the parent's environment stay intact.
        let file = tempfile::NamedTempFile::new().expect("unavailable workspace fixture");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD_WORKSPACE, file.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("workspace failure child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.try_wait().expect("child status").is_none() {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("workspace failure child timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().expect("child output");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!("test {test_name} ... ok")),
            "the exact child test must run, not just exit successfully with zero tests"
        );
        false
    }
}
