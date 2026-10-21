use super::*;
use crate::auth::now_epoch;
use std::sync::atomic::{AtomicBool, Ordering};

fn member_repository(access: RepositoryAccess) -> RepositoryConfig {
    RepositoryConfig {
        owner: "team".into(),
        name: "private".into(),
        bucket: "bucket".into(),
        prefix: "private".into(),
        default_branch: "main".into(),
        description: String::new(),
        members: vec![crate::RepositoryMember {
            subject: "alice".into(),
            name: "Alice".into(),
            access,
        }],
        protected_branches: vec![],
    }
}

fn active_session() -> Arc<Session> {
    Arc::new(Session {
        identity: Identity {
            issuer: "https://identity.example".into(),
            subject: "alice".into(),
            name: "Alice".into(),
        },
        csrf: "csrf".into(),
        expires_at: now_epoch().unwrap() + 60,
        session_key: [0; 32],
        revoked: AtomicBool::new(false),
    })
}

#[test]
fn repository_administration_stays_out_of_write_scoped_git_tokens() {
    let repository = member_repository(RepositoryAccess::Admin);
    let session = active_session();
    let browser = Principal::user(Arc::clone(&session)).with_membership(
        Uuid::from_u128(1),
        &repository,
        "https://identity.example",
    );
    assert!(browser.can_read(Uuid::from_u128(1)));
    assert!(browser.can_write(Uuid::from_u128(1)));
    assert!(browser.can_admin(Uuid::from_u128(1)));

    let git = Principal::git(Arc::new(GitToken {
        repository_id: uuid::Uuid::from_u128(1),
        session,
        owner: repository.owner.clone(),
        repository: repository.name.clone(),
        access: RepositoryAccess::Write,
        revoked: AtomicBool::new(false),
    }));
    let git = git.with_membership(Uuid::from_u128(1), &repository, "https://identity.example");
    assert!(git.can_write(Uuid::from_u128(1)));
    assert!(!git.can_admin(Uuid::from_u128(1)));
    assert!(
        !Principal::user(active_session())
            .with_membership(
                Uuid::from_u128(1),
                &member_repository(RepositoryAccess::Write),
                "https://identity.example"
            )
            .can_admin(Uuid::from_u128(1))
    );
}

#[test]
fn triage_manages_collaboration_without_code_write_or_repository_administration() {
    let principal = Principal::user(active_session());
    for (access, triage, write, admin) in [
        (RepositoryAccess::Read, false, false, false),
        (RepositoryAccess::Triage, true, false, false),
        (RepositoryAccess::Write, true, true, false),
        (RepositoryAccess::Admin, true, true, true),
    ] {
        let repo = member_repository(access);
        let principal =
            principal.with_membership(Uuid::from_u128(1), &repo, "https://identity.example");
        assert_eq!(
            (
                principal.can_read(Uuid::from_u128(1)),
                principal.can_triage(Uuid::from_u128(1)),
                principal.can_write(Uuid::from_u128(1)),
                principal.can_admin(Uuid::from_u128(1))
            ),
            (true, triage, write, admin),
            "{access:?}"
        );
    }
    let token = Principal::git(Arc::new(GitToken {
        repository_id: uuid::Uuid::from_u128(1),
        session: active_session(),
        owner: "team".into(),
        repository: "private".into(),
        access: RepositoryAccess::Read,
        revoked: AtomicBool::new(false),
    }));
    assert!(
        !token
            .with_membership(
                Uuid::from_u128(1),
                &member_repository(RepositoryAccess::Triage),
                "https://identity.example"
            )
            .can_triage(Uuid::from_u128(1))
    );
}

#[test]
fn credentials_need_a_grant_and_grants_cannot_retarget_another_repository_uuid() {
    let principal = Principal::user(active_session());
    let id = Uuid::from_u128(1);
    assert!(!principal.can_read(id));
    let admitted = principal.with_membership(
        id,
        &member_repository(RepositoryAccess::Admin),
        "https://identity.example",
    );
    assert_eq!(
        (admitted.can_read(id), admitted.can_read(Uuid::from_u128(2))),
        (true, false)
    );
}

#[test]
fn a_matching_subject_from_another_issuer_never_receives_a_repository_grant() {
    let principal = Principal::user(active_session());
    let id = Uuid::from_u128(1);
    let admitted = principal.with_membership(
        id,
        &member_repository(RepositoryAccess::Admin),
        "https://another.example",
    );
    assert!(!admitted.can_read(id));
}

#[test]
fn a_write_token_cannot_raise_a_triage_member_to_code_write_access() {
    let principal = Principal::git(Arc::new(GitToken {
        repository_id: uuid::Uuid::from_u128(1),
        session: active_session(),
        owner: "team".into(),
        repository: "private".into(),
        access: RepositoryAccess::Write,
        revoked: AtomicBool::new(false),
    }));
    let id = Uuid::from_u128(1);
    let admitted = principal.with_membership(
        id,
        &member_repository(RepositoryAccess::Triage),
        "https://identity.example",
    );
    assert_eq!(
        (
            admitted.can_triage(id),
            admitted.can_write(id),
            admitted.can_admin(id)
        ),
        (true, false, false)
    );
}

#[test]
fn revoking_a_session_invalidates_its_already_admitted_grant() {
    let session = active_session();
    let principal = Principal::user(Arc::clone(&session));
    let id = Uuid::from_u128(1);
    let admitted = principal.with_membership(
        id,
        &member_repository(RepositoryAccess::Admin),
        "https://identity.example",
    );
    assert!(admitted.can_read(id));
    session.revoked.store(true, Ordering::Release);
    assert!(!admitted.can_read(id));
}

#[test]
fn a_token_cannot_resolve_new_scope_for_a_repository_that_reuses_its_name() {
    let principal = Principal::git(Arc::new(GitToken {
        repository_id: Uuid::from_u128(1),
        session: active_session(),
        owner: "team".into(),
        repository: "private".into(),
        access: RepositoryAccess::Write,
        revoked: AtomicBool::new(false),
    }));
    let replacement = Uuid::from_u128(2);
    let admitted = principal.with_membership(
        replacement,
        &member_repository(RepositoryAccess::Admin),
        "https://identity.example",
    );
    assert!(!admitted.can_read(replacement));
}
