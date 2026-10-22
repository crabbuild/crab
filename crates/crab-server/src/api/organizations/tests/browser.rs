use super::*;
use crate::cells::organization::{ChangeMember, ChangeOutcome, MemberChange, Role};
use std::time::{Duration, Instant};

struct Browser(String);
impl Browser {
    fn new(name: &str) -> Self {
        Self(format!("crab-org-{name}-{}", Uuid::now_v7().simple()))
    }
    async fn run(&self, arguments: &[&str]) -> String {
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cell fixtures"]
async fn native_browser_invitation_consent_and_removed_member_privacy() {
    let h = Harness::new(false).await;
    let (id, alice, bob) = crate::test_support::http::organizations::bootstrap(&h).await;
    let owner = Browser::new("owner");
    let invitee = Browser::new("invitee");
    let root = format!("{}/-/organizations/{id}", h.origin);
    owner.run(&["open", &root]).await;
    owner
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Continue to sign in",
            "--exact",
        ])
        .await;
    owner.run(&["wait", "--text", "Organization members"]).await;
    *h.provider.mode.lock().await = "member".into();
    invitee
        .run(&["open", &format!("{}/-/profile", h.origin)])
        .await;
    invitee
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Continue to sign in",
            "--exact",
        ])
        .await;
    invitee.run(&["wait", "--text", "Public profile"]).await;
    invitee
        .run(&["find", "label", "Username", "fill", "bob-qualification"])
        .await;
    invitee
        .run(&["find", "label", "Display name", "fill", "Bob via browser"])
        .await;
    invitee
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Create profile",
            "--exact",
        ])
        .await;
    invitee.run(&["wait", "--text", "Profile created."]).await;
    owner
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Invitations",
            "--exact",
        ])
        .await;
    owner
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Invite member",
            "--exact",
        ])
        .await;
    owner
        .run(&["find", "label", "Username", "fill", "bob-qualification"])
        .await;
    owner.run(&["wait", "--text", "Bob via browser"]).await;
    owner
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Bob via browser @bob-qualification",
            "--exact",
        ])
        .await;
    owner.run(&["wait", "button[type='submit']:enabled"]).await;
    owner
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Create invitation",
            "--exact",
        ])
        .await;
    owner.run(&["wait", "--text", "Invitation created."]).await;
    let cells = h.server.cells().unwrap();
    let before = cells
        .query::<ListMembers>(
            Address::Organization(id),
            None,
            PageQuery {
                actor: alice,
                after: None,
                limit: 50,
            },
        )
        .await
        .unwrap()
        .output;
    let Page::Page { items, .. } = before else {
        panic!("page changed")
    };
    assert_eq!(items.len(), 1);
    invitee.run(&["open", &format!("{root}/invitation")]).await;
    invitee
        .run(&["wait", "--text", "Invitation to Crab engineering"])
        .await;
    invitee
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Join organization",
            "--exact",
        ])
        .await;
    invitee
        .run(&["wait", "--text", "You accepted this invitation."])
        .await;
    invitee
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "View organization",
            "--exact",
        ])
        .await;
    invitee
        .run(&["wait", "--text", "Organization members"])
        .await;
    invitee.run(&["wait", "--text", "Bob via browser"]).await;
    let after = cells
        .query::<ListMembers>(
            Address::Organization(id),
            None,
            PageQuery {
                actor: alice,
                after: None,
                limit: 50,
            },
        )
        .await
        .unwrap()
        .output;
    let Page::Page { items, .. } = after else {
        panic!("page changed")
    };
    let member = items.iter().find(|member| member.actor == bob).unwrap();
    assert_eq!((items.len(), member.role), (2, Role::Member));
    let controls = invitee.run(&["snapshot", "-i"]).await;
    assert!(!controls.contains("Remove"));
    let removed = cells
        .command::<ChangeMember>(
            Address::Organization(id),
            app::mutation_identity().unwrap(),
            MemberChange {
                actor: alice,
                member: bob,
                version: member.version,
                role: None,
            },
        )
        .await
        .unwrap()
        .output;
    assert!(matches!(removed, ChangeOutcome::Applied(_)));
    invitee.run(&["open", &format!("{root}/invitation")]).await;
    invitee
        .run(&["wait", "--text", "Organization invitation"])
        .await;
    let history = invitee.run(&["snapshot"]).await;
    assert!(
        history.contains("You accepted this invitation.")
            && !history.contains("Crab engineering")
            && !history.contains("View organization")
    );
    drop(invitee);
    drop(owner);
    h.close().await;
}
