use std::sync::Arc;

use object_store::memory::InMemory;
use sha2::Digest as _;

use super::*;

mod read;

fn catalog() -> CatalogStore {
    let store = crab_storage::Store::new(Arc::new(InMemory::new()));
    CatalogStore::new(StorageRoot::memory(store, "repositories"))
}

fn actor() -> MembershipActor {
    MembershipActor {
        issuer: "https://issuer.example".into(),
        subject: "alice-id".into(),
    }
}

#[tokio::test]
async fn group_only_access_edits_are_audited_and_member_commands_preserve_them() {
    let catalog = catalog();
    catalog
        .create_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            "main".into(),
            String::new(),
            vec![admin()],
        )
        .await
        .unwrap();
    let before = catalog.membership("team", "project").await.unwrap();
    let groups = vec![RepositoryGroup {
        group: "/Engineering".into(),
        access: crate::RepositoryAccess::Write,
    }];
    let saved = catalog
        .replace_access(
            "team",
            "project",
            AccessUpdate {
                expected_revision: before.revision,
                members: before.members,
                groups: groups.clone(),
            },
            actor(),
            true,
        )
        .await
        .unwrap();
    let (after, _) = catalog.load().await.unwrap();
    let head = after.membership_audit_head.unwrap();
    let (body, _) = catalog
        .root
        .store
        .get_with_etag_bounded(&catalog.root.path(&head), 8192)
        .await
        .unwrap();
    let event: MembershipAuditEvent = serde_json::from_slice(&body).unwrap();
    assert_ne!(event.previous_members_digest, event.new_members_digest);
    assert_eq!(saved.groups, groups);
    let mut renamed = admin();
    renamed.name = "Alice renamed".into();
    catalog
        .set_members("team", "project", vec![renamed], true)
        .await
        .unwrap();
    assert_eq!(
        catalog.membership("team", "project").await.unwrap().groups,
        groups
    );
}

fn admin() -> RepositoryMember {
    RepositoryMember {
        subject: "alice-id".into(),
        name: "Alice".into(),
        access: crate::RepositoryAccess::Admin,
    }
}

#[tokio::test]
async fn membership_requires_current_revision_and_final_admin_without_writes() {
    let catalog = catalog();
    catalog
        .create_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            "main".into(),
            String::new(),
            vec![admin()],
        )
        .await
        .unwrap();
    let (before, etag) = catalog.load().await.unwrap();
    assert!(matches!(
        catalog
            .replace_access(
                "team",
                "project",
                AccessUpdate {
                    expected_revision: before.version - 1,
                    members: vec![admin()],
                    groups: vec![]
                },
                actor(),
                true
            )
            .await,
        Err(CatalogError::Conflict)
    ));
    assert!(matches!(
        catalog
            .replace_access(
                "team",
                "project",
                AccessUpdate {
                    expected_revision: before.version,
                    members: vec![],
                    groups: vec![]
                },
                actor(),
                true
            )
            .await,
        Err(CatalogError::Invalid(
            "at least one administrator is required"
        ))
    ));
    assert_eq!(catalog.load().await.unwrap(), (before, etag));
}

#[tokio::test]
async fn pending_audit_survives_crash_and_later_mutation_keeps_chain() {
    let catalog = catalog();
    let record = catalog
        .create_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            "main".into(),
            String::new(),
            vec![admin()],
        )
        .await
        .unwrap();
    let (mut document, etag) = catalog.load().await.unwrap();
    let head = document.membership_audit_head.clone().unwrap();
    let (body, _) = catalog
        .root
        .store
        .get_with_etag_bounded(&catalog.root.path(&head), 8192)
        .await
        .unwrap();
    let event: MembershipAuditEvent = serde_json::from_slice(&body).unwrap();
    // Recreate the committed outbox state immediately before its flush.
    catalog
        .root
        .store
        .delete(&catalog.root.path(&head))
        .await
        .unwrap();
    document.membership_audit_head = None;
    document.pending_membership_audit = Some(event.clone().into());
    assert!(catalog.write_document(&document, etag).await.unwrap());
    let mut member = admin();
    member.name = "Alice renamed".into();
    catalog
        .replace_access(
            "team",
            "project",
            AccessUpdate {
                expected_revision: document.version,
                members: vec![member],
                groups: vec![],
            },
            actor(),
            true,
        )
        .await
        .unwrap();
    catalog.flush_membership_audit().await.unwrap();
    let (after, etag) = catalog.load().await.unwrap();
    catalog.flush_membership_audit().await.unwrap();
    assert_eq!(catalog.load().await.unwrap(), (after.clone(), etag));
    let current_head = after.membership_audit_head.unwrap();
    let (body, _) = catalog
        .root
        .store
        .get_with_etag_bounded(&catalog.root.path(&current_head), 8192)
        .await
        .unwrap();
    let current: MembershipAuditEvent = serde_json::from_slice(&body).unwrap();
    assert_eq!(current.repository_id, Some(record.id));
    let previous = current.previous.unwrap();
    let (body, _) = catalog
        .root
        .store
        .get_with_etag_bounded(&catalog.root.path(&previous), 8192)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<MembershipAuditEvent>(&body).unwrap(),
        event
    );
}

#[tokio::test]
async fn membership_audit_chain_survives_storage_root_relocation() {
    let store = crab_storage::Store::new(Arc::new(InMemory::new()));
    let source = CatalogStore::new(StorageRoot::memory(store.clone(), "repositories/source"));
    source
        .create_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            "main".into(),
            String::new(),
            vec![admin()],
        )
        .await
        .unwrap();
    let (document, _) = source.load().await.unwrap();
    let head = document.membership_audit_head.clone().unwrap();
    assert!(head.starts_with(MEMBERSHIP_AUDIT_RELATIVE_PREFIX));
    let (catalog_body, _) = source
        .root
        .store
        .get_with_etag_bounded(&source.path, MAX_CATALOG_BYTES)
        .await
        .unwrap();
    let (audit, _) = source
        .root
        .store
        .get_with_etag_bounded(&source.root.path(&head), MAX_CATALOG_BYTES)
        .await
        .unwrap();

    let restored_root = StorageRoot::memory(store, "repositories/restored");
    restored_root
        .store
        .put_overwrite(&restored_root.path(CATALOG_RELATIVE_PATH), catalog_body)
        .await
        .unwrap();
    let restored = CatalogStore::new(restored_root.clone());
    restored
        .root
        .store
        .put_overwrite(&restored.root.path(&head), audit)
        .await
        .unwrap();
    restored
        .root
        .store
        .put_overwrite(
            &restored.path,
            Bytes::from(serde_json::to_vec(&document).unwrap()),
        )
        .await
        .unwrap();

    assert_eq!(restored.load().await.unwrap().0, document);

    let mut invalid = document;
    invalid.membership_audit_head = Some(source.root.path(&head).to_string());
    restored_root
        .store
        .put_overwrite(
            &restored.path,
            Bytes::from(serde_json::to_vec(&invalid).unwrap()),
        )
        .await
        .unwrap();
    assert!(matches!(
        restored.load().await,
        Err(CatalogError::Invalid(_))
    ));
}

#[test]
fn membership_digest_is_independent_of_presentation_order() {
    let first = admin();
    let second = RepositoryMember {
        subject: "bob-id".into(),
        name: "Bob".into(),
        access: crate::RepositoryAccess::Read,
    };
    assert_eq!(
        membership_digest(&[first.clone(), second.clone()], &[], None).unwrap(),
        membership_digest(&[second, first], &[], None).unwrap()
    );
}

#[tokio::test]
async fn create_initializes_and_publishes_one_repository() {
    let catalog = catalog();
    let record = catalog
        .create_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            "main".into(),
            "Project".into(),
            vec![],
        )
        .await
        .unwrap();
    let (document, _) = catalog.load().await.unwrap();
    assert_eq!(document.repositories, [record]);
    assert_eq!(document.version, 1);
    assert_eq!(
        document.repositories[0].application,
        RepositoryApplicationState::EmptyCellPending
    );
}

#[tokio::test]
async fn adopt_requires_canonical_repository_metadata() {
    let catalog = catalog();
    let result = catalog
        .adopt_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            String::new(),
            vec![],
        )
        .await;
    assert!(matches!(result, Err(CatalogError::Read(_))));
}

#[tokio::test]
async fn adopt_rejects_a_v2_root_with_missing_scoped_pointer_data() {
    let catalog = catalog();
    let layout = catalog
        .root
        .repository_layout(catalog.root.repository_prefix("team/project").unwrap());
    let base = crab_write::capsule_protocol::initialize(
        &layout,
        blake3::hash(b"team-project").to_hex().as_ref(),
        "refs/heads/main",
    )
    .await
    .unwrap();
    let mut pointers = crab_metadata::capsule_protocol::PointerCatalog::new();
    pointers
        .insert_xorb(
            "1".repeat(64),
            crab_metadata::capsule_protocol::XorbCatalogEntry::new(
                1,
                "2".repeat(64),
                vec![crab_metadata::capsule_protocol::XorbChunkEntry::new(
                    "3".repeat(64),
                    1,
                )],
            ),
        )
        .unwrap();
    let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::new(
        base.record().digest(),
        vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some("4".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let capsule = crab_metadata::capsule_protocol::Capsule::build(
        &transaction,
        vec![
            crab_metadata::capsule_protocol::CapsuleGitPack::new(
                Bytes::from_static(b"pack"),
                Bytes::from_static(b"index"),
                Bytes::from_static(b"reverse"),
                Bytes::from_static(b"locator"),
                "5".repeat(40),
                1,
            )
            .unwrap(),
        ],
        vec![crab_metadata::capsule_protocol::CapsuleSection::new(
            crab_metadata::capsule_protocol::CapsuleSectionKind::CatalogDelta,
            pointers.encode_delta().unwrap(),
        )],
    )
    .unwrap();
    crab_write::capsule_protocol::publish(&layout, base, &transaction, capsule)
        .await
        .unwrap();

    let result = catalog
        .adopt_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            String::new(),
            vec![],
        )
        .await;

    assert!(matches!(result, Err(CatalogError::Read(_))));
    assert!(catalog.load().await.unwrap().0.repositories.is_empty());
}

#[tokio::test]
async fn adopt_rejects_a_reachable_lfs_pointer_without_its_object() {
    let catalog = catalog();
    let layout = catalog
        .root
        .repository_layout(catalog.root.repository_prefix("team/project").unwrap());
    let pointer = crab_git::LfsPointer {
        oid: [0x42; 32],
        size: 7,
        extensions: Vec::new(),
    };
    crate::test_support::git::publish_blob(&layout, &pointer.serialize()).await;

    let result = catalog
        .adopt_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            String::new(),
            vec![],
        )
        .await;

    assert!(matches!(
        result,
        Err(CatalogError::Read(crab_read::ReadError::Lfs(
            crab_lfs::LfsError::ObjectMissing { .. }
        )))
    ));
    assert!(catalog.load().await.unwrap().0.repositories.is_empty());
}

#[tokio::test]
async fn adopt_rejects_a_reachable_crab_pointer_absent_from_the_catalog() {
    let catalog = catalog();
    let layout = catalog
        .root
        .repository_layout(catalog.root.repository_prefix("team/project").unwrap());
    let pointer = format!(
        "version https://crab.build/spec/v1\nfile-hash {}\nsize 7\n",
        "31".repeat(32)
    );
    crate::test_support::git::publish_blob(&layout, pointer.as_bytes()).await;

    let result = catalog
        .adopt_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            String::new(),
            vec![],
        )
        .await;

    assert!(matches!(
        result,
        Err(CatalogError::Read(crab_read::ReadError::CorruptObject {
            reason,
            ..
        })) if reason.contains("absent from the catalog")
    ));
    assert!(catalog.load().await.unwrap().0.repositories.is_empty());
}

#[tokio::test]
async fn adopt_accepts_a_reachable_lfs_pointer_with_verified_content() {
    let catalog = catalog();
    let layout = catalog
        .root
        .repository_layout(catalog.root.repository_prefix("team/project").unwrap());
    let content = Bytes::from_static(b"content");
    let oid: [u8; 32] = sha2::Sha256::digest(&content).into();
    let pointer = crab_git::LfsPointer {
        oid,
        size: content.len() as u64,
        extensions: Vec::new(),
    };
    crate::test_support::git::publish_blob(&layout, &pointer.serialize()).await;
    crab_lfs::LfsObjectStore::new(layout.store().clone(), layout.repo_prefix())
        .put(&oid, content)
        .await
        .unwrap();

    let result = catalog
        .adopt_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            String::new(),
            vec![],
        )
        .await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn adopted_repository_requires_empty_cell_and_ready_transition_is_idempotent() {
    let catalog = catalog();
    let layout = catalog
        .root
        .repository_layout(catalog.root.repository_prefix("team/project").unwrap());
    let repository_id = blake3::hash(b"team-project").to_hex().to_string();
    crab_write::capsule_protocol::initialize(&layout, &repository_id, "refs/heads/main")
        .await
        .unwrap();
    let runtime = catalog
        .adopt_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            String::new(),
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(
        runtime.application,
        RepositoryApplicationState::EmptyCellPending
    );

    let ready = catalog.mark_cell_ready(runtime.id).await.unwrap();
    let repeated = catalog.mark_cell_ready(runtime.id).await.unwrap();
    let (document, _) = catalog.load().await.unwrap();

    assert_eq!(ready.application, RepositoryApplicationState::CellReady);
    assert_eq!(repeated, ready);
    assert_eq!(document.version, 2);
}

#[tokio::test]
async fn unsupported_catalog_schemas_are_rejected() {
    let catalog = catalog();
    for schema_version in [1, 2, 4] {
        let document = CatalogDocument {
            schema_version,
            ..Default::default()
        };
        catalog
            .root
            .store
            .put_overwrite(
                &catalog.path,
                Bytes::from(serde_json::to_vec(&document).unwrap()),
            )
            .await
            .unwrap();
        assert!(
            matches!(catalog.load().await, Err(CatalogError::Invalid(_))),
            "schema {schema_version}"
        );
    }
}

#[tokio::test]
async fn retrying_create_returns_the_existing_record() {
    let catalog = catalog();
    let first = catalog
        .create_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            "main".into(),
            String::new(),
            vec![],
        )
        .await
        .unwrap();
    let second = catalog
        .create_repository(
            "TEAM".into(),
            "PROJECT".into(),
            "team/project".into(),
            "main".into(),
            String::new(),
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(first, second);
}

async fn reserve_personal(
    catalog: &CatalogStore,
    namespace: BrowserNamespace,
    name: &str,
) -> Result<CatalogRecord, CatalogError> {
    catalog
        .reserve_browser_repository(
            namespace,
            BrowserCreation {
                request_id: Uuid::now_v7(),
                name: name.into(),
                description: String::new(),
                visibility: visibility::Visibility::Private,
                properties: BTreeMap::new(),
                add_readme: false,
                gitignore_template: None,
                license_template: None,
            },
            actor(),
            Vec::new(),
        )
        .await
}

#[tokio::test]
async fn personal_namespaces_belong_to_one_account() {
    let catalog = catalog();
    let account = |owner: &str, subject: &str| BrowserNamespace::ManagedPersonal {
        owner: owner.into(),
        actor: MembershipActor {
            issuer: actor().issuer,
            subject: subject.into(),
        },
    };
    reserve_personal(&catalog, account("alice", "alice-id"), "notes")
        .await
        .unwrap();
    reserve_personal(&catalog, account("alice", "alice-id"), "tools")
        .await
        .unwrap();
    reserve_personal(
        &catalog,
        BrowserNamespace::Personal("team".into()),
        "project",
    )
    .await
    .unwrap();
    for namespace in [
        account("alice", "bob-id"),
        account("ALICE", "bob-id"),
        BrowserNamespace::Personal("alice".into()),
        account("team", "alice-id"),
    ] {
        assert!(matches!(
            reserve_personal(&catalog, namespace, "claimed").await,
            Err(CatalogError::Conflict)
        ));
    }
    for owner in ["api", organization_namespace(Uuid::now_v7()).as_str()] {
        assert!(matches!(
            reserve_personal(&catalog, account(owner, "alice-id"), "routed").await,
            Err(CatalogError::Invalid(_))
        ));
    }
}

#[tokio::test]
async fn import_reservation_rejects_an_existing_repository() {
    let catalog = catalog();
    catalog
        .create_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            "main".into(),
            String::new(),
            vec![],
        )
        .await
        .unwrap();
    let result = catalog
        .reserve_git_import(
            BrowserNamespace::Personal("team".into()),
            GitImportReservation {
                id: Uuid::now_v7(),
                name: "project".into(),
                description: String::new(),
                visibility: visibility::Visibility::Private,
                forked_from: None,
                mirror: None,
            },
            MembershipActor {
                issuer: "issuer".into(),
                subject: "author".into(),
            },
            vec![],
        )
        .await;
    assert!(matches!(result, Err(CatalogError::Conflict)));
}

#[tokio::test]
async fn membership_replacement_is_atomic_and_idempotent() {
    let catalog = catalog();
    catalog
        .create_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            "main".into(),
            String::new(),
            vec![],
        )
        .await
        .unwrap();
    let members = vec![RepositoryMember {
        subject: "alice-sub".into(),
        name: "Alice".into(),
        access: crate::RepositoryAccess::Admin,
    }];

    let updated = catalog
        .set_members("TEAM", "PROJECT", members.clone(), true)
        .await
        .unwrap();
    let (after_update, _) = catalog.load().await.unwrap();
    let repeated = catalog
        .set_members("team", "project", members, true)
        .await
        .unwrap();
    let (after_repeat, _) = catalog.load().await.unwrap();

    assert_eq!(updated, repeated);
    assert_eq!(after_update.version, 2);
    assert_eq!(after_repeat, after_update);
}

#[tokio::test]
async fn membership_replacement_requires_a_cataloged_repository() {
    let result = catalog()
        .set_members("team", "missing", vec![], false)
        .await;

    assert!(matches!(result, Err(CatalogError::NotFound)));
}

#[tokio::test]
async fn invalid_membership_cannot_change_the_catalog() {
    let catalog = catalog();
    catalog
        .create_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            "main".into(),
            String::new(),
            vec![],
        )
        .await
        .unwrap();
    let (before, _) = catalog.load().await.unwrap();
    let member = RepositoryMember {
        subject: "duplicate-subject".into(),
        name: "Alice".into(),
        access: crate::RepositoryAccess::Admin,
    };

    let result = catalog
        .set_members(
            "team",
            "project",
            vec![
                member.clone(),
                RepositoryMember {
                    name: "Bob".into(),
                    ..member
                },
            ],
            true,
        )
        .await;
    let (after, _) = catalog.load().await.unwrap();

    assert!(matches!(result, Err(CatalogError::Invalid(_))));
    assert_eq!(after, before);
}

#[tokio::test]
async fn repository_prefixes_cannot_alias_or_enter_server_state() {
    let catalog = catalog();
    for prefix in ["/team/project/", ".crab/http-server/v1/auth"] {
        let result = catalog
            .adopt_repository(
                "team".into(),
                "project".into(),
                prefix.into(),
                String::new(),
                vec![],
            )
            .await;
        assert!(matches!(result, Err(CatalogError::Invalid(_))), "{prefix}");
    }
}

#[tokio::test]
async fn organization_audit_retains_each_side_of_a_transfer_and_survives_root_restore() {
    let catalog = catalog();
    let former = Uuid::now_v7();
    let current = Uuid::now_v7();
    let runtime = catalog
        .create_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            "main".into(),
            String::new(),
            vec![admin()],
        )
        .await
        .unwrap();
    let before = catalog.load().await.unwrap().0;
    assert!(before.organization_audit_start(former).is_none());
    let legacy = before.membership_audit_head.unwrap();
    catalog.mark_cell_ready(runtime.id).await.unwrap();
    let revision = catalog.load().await.unwrap().0.version;
    catalog
        .set_organization_access(
            "team",
            "project",
            runtime.id,
            revision,
            OrganizationAccess {
                id: former,
                teams: vec![],
            },
            actor(),
        )
        .await
        .unwrap();
    let before = catalog.load().await.unwrap().0;
    let former_history = before.organization_audit_start(former).unwrap();
    let offer = Uuid::now_v7();
    let now = 1_800_000_000_000;
    let pending = catalog
        .propose_transfer(
            before.version,
            runtime.id,
            transfer::Proposal {
                id: offer,
                requester: actor(),
                recipient: transfer::Recipient::Organization { id: current },
                destination_handle: None,
                recipient_label: "Destination".into(),
                name: "renamed-on-transfer".into(),
                version: 0,
            },
            now,
        )
        .await
        .unwrap();
    catalog
        .decide_transfer(
            pending.version,
            offer,
            transfer::Decision::Accept {
                actor: actor(),
                display_name: "Alice".into(),
                property_seed: None,
            },
            now + 2000,
        )
        .await
        .unwrap();
    let document = catalog.load().await.unwrap().0;
    let shared = document.organization_audit_start(former).unwrap();
    assert_eq!(document.organization_audit_start(current), Some(shared));
    let restored = CatalogStore::new(StorageRoot::memory(catalog.root.store.clone(), "restored"));
    for path in [
        CATALOG_RELATIVE_PATH.to_string(),
        legacy,
        former_history.path(),
        shared.path(),
    ] {
        let (bytes, _) = catalog
            .root
            .store
            .get_with_etag_bounded(&catalog.root.path(&path), MAX_CATALOG_BYTES)
            .await
            .unwrap();
        restored
            .root
            .store
            .put_overwrite(&restored.root.path(&path), bytes)
            .await
            .unwrap();
    }

    let (snapshot, _) = restored.load_snapshot().await.unwrap();
    let outgoing = restored
        .organization_audit_entry(&snapshot, former, shared)
        .await
        .unwrap();
    assert_eq!(
        (
            outgoing.action,
            outgoing.repository.as_ref().map(|(_, name)| name.as_str()),
            outgoing.previous
        ),
        (
            audit::Action::TransferredOut,
            Some("project"),
            Some(former_history)
        )
    );
    let incoming = restored
        .organization_audit_entry(&snapshot, current, shared)
        .await
        .unwrap();
    assert_eq!(
        (
            incoming.action,
            incoming.repository.as_ref().map(|(_, name)| name.as_str()),
            incoming.previous
        ),
        (
            audit::Action::TransferredIn,
            Some("renamed-on-transfer"),
            None
        )
    );
    let old = restored
        .organization_audit_entry(&snapshot, former, former_history)
        .await
        .unwrap();
    assert_eq!(old.previous, None);
    assert!(
        restored
            .organization_audit_entry(&snapshot, current, former_history)
            .await
            .is_err()
    );
}
