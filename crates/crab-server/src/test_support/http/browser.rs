use std::time::{Duration, Instant};
use uuid::Uuid;

pub(crate) struct Browser(String);
impl Browser {
    pub(crate) fn new(name: &str) -> Self {
        Self(format!("crab-test-{name}-{}", Uuid::now_v7().simple()))
    }
    pub(crate) async fn sign_in(&self) {
        self.run(&["wait", "--text", "Continue to sign in"]).await;
        self.run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Continue to sign in",
            "--exact",
        ])
        .await;
    }

    pub(crate) async fn run(&self, arguments: &[&str]) -> String {
        let output = tokio::time::timeout(
            Duration::from_secs(45),
            tokio::process::Command::new("agent-browser")
                .args(["--session", &self.0])
                .args(arguments)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        if !output.status.success() {
            let snapshot = tokio::process::Command::new("agent-browser")
                .args(["--session", &self.0, "snapshot"])
                .output()
                .await
                .unwrap();
            panic!(
                "{:?}: {}\n{}",
                arguments,
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&snapshot.stdout)
            );
        }
        String::from_utf8(output.stdout).unwrap()
    }
}
impl Drop for Browser {
    fn drop(&mut self) {
        // Failed qualification must close only its own browser, before fixture
        // storage disappears. Bound cleanup even if its daemon stopped replying.
        let Ok(mut child) = std::process::Command::new("agent-browser")
            .args(["--session", &self.0, "close"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if child.try_wait().is_ok_and(|value| value.is_some()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}
