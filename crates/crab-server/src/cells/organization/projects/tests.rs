use super::*;
use crate::cells::organization::{
    ChangeMember, ChangeOutcome, DecideInvitation, Decision, Invite, InviteInput, MemberChange,
};
use crate::cells::{
    Address, CellClient,
    repository::{
        collaboration::{Subject, SubjectKind},
        projects::Item as PlanningItem,
    },
};
use crate::test_support::server as fixtures;
use std::sync::Arc;

async fn write<M: Mutation>(
    client: &CellClient,
    organization: Uuid,
    input: M::Input,
) -> Result<M::Output> {
    Ok(client
        .command::<M>(
            Address::Organization(organization),
            crate::app::mutation_identity().unwrap(),
            input,
        )
        .await?
        .output)
}
async fn setup() -> (Arc<crate::server::Server>, Uuid, Uuid, Uuid) {
    let server = fixtures::fixture().await;
    let client = &server.cells().unwrap();
    let organization = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let project = Uuid::now_v7();
    client
        .initialize(Address::Organization(organization))
        .await
        .unwrap();
    write::<super::super::Bootstrap>(
        client,
        organization,
        super::super::BootstrapInput {
            actor: owner,
            name: "Engineering".into(),
            description: String::new(),
        },
    )
    .await
    .unwrap();
    let result = write::<Change>(
        client,
        organization,
        request(
            owner,
            Command::Create {
                id: project,
                name: "Release".into(),
                template: "board".into(),
            },
        ),
    )
    .await
    .unwrap();
    assert!(matches!(result, Outcome::Applied { version: 1, .. }));
    (server, organization, owner, project)
}
fn request(actor: Uuid, command: Command) -> Request {
    Request {
        actor,
        request_id: Uuid::now_v7(),
        command,
        cancel: false,
        source_allowed: true,
    }
}
fn work(repository: Uuid) -> Item {
    Item {
        repository: Some(repository),
        item: PlanningItem {
            id: Uuid::now_v7(),
            subject: Some(Subject {
                kind: SubjectKind::Issue,
                number: 1,
            }),
            title: String::new(),
            body: String::new(),
            status: "Todo".into(),
            priority: 0,
            start: None,
            end: None,
            values: BTreeMap::new(),
        },
    }
}
async fn link(
    client: &CellClient,
    organization: Uuid,
    owner: Uuid,
    project: Uuid,
    version: u64,
    repository: Uuid,
) {
    let result = write::<Change>(
        client,
        organization,
        request(
            owner,
            Command::Repository {
                project,
                version,
                repository,
                linked: true,
            },
        ),
    )
    .await
    .unwrap();
    assert!(matches!(result, Outcome::Applied { .. }));
}
async fn items(
    client: &CellClient,
    organization: Uuid,
    owner: Uuid,
    project: Uuid,
) -> reads::ItemPage {
    client
        .query::<Items>(
            Address::Organization(organization),
            None,
            ItemsQuery {
                actor: owner,
                project,
                after: None,
                version: None,
                status: None,
                repository: None,
                item: None,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap()
}
#[tokio::test]
async fn same_issue_number_in_two_repositories_is_distinct_work() {
    let (server, organization, owner, project) = setup().await;
    let client = &server.cells().unwrap();
    let a = Uuid::now_v7();
    let b = Uuid::now_v7();
    link(client, organization, owner, project, 1, a).await;
    link(client, organization, owner, project, 2, b).await;
    for record in [work(a), work(b)] {
        write::<Change>(
            client,
            organization,
            request(
                owner,
                Command::Item {
                    project,
                    version: 0,
                    repository: record.repository,
                    item: record.item,
                },
            ),
        )
        .await
        .unwrap();
    }
    let page = items(client, organization, owner, project).await;
    assert_eq!(
        page.items
            .iter()
            .filter_map(|item| item.record.repository)
            .collect::<std::collections::BTreeSet<_>>(),
        [a, b].into()
    );
    fixtures::close(&server).await;
}
#[tokio::test]
async fn cancellation_fences_a_late_original_submission() {
    let (server, organization, owner, project) = setup().await;
    let client = &server.cells().unwrap();
    let original = request(
        owner,
        Command::Repository {
            project,
            version: 1,
            repository: Uuid::now_v7(),
            linked: true,
        },
    );
    let mut cancelled = original.clone();
    cancelled.cancel = true;
    write::<Change>(client, organization, cancelled)
        .await
        .unwrap();
    assert_eq!(
        write::<Change>(client, organization, original)
            .await
            .unwrap(),
        Outcome::Cancelled
    );
    fixtures::close(&server).await;
}
#[tokio::test]
async fn rejected_source_authority_cannot_execute_after_access_is_granted() {
    let (server, organization, owner, project) = setup().await;
    let client = &server.cells().unwrap();
    let mut input = request(
        owner,
        Command::Repository {
            project,
            version: 1,
            repository: Uuid::now_v7(),
            linked: true,
        },
    );
    input.source_allowed = false;
    let old = write::<Change>(client, organization, input.clone())
        .await
        .unwrap();
    input.source_allowed = true;
    assert_eq!(
        write::<Change>(client, organization, input).await.unwrap(),
        old
    );
    fixtures::close(&server).await;
}
#[tokio::test]
async fn unlinking_a_repository_cannot_erase_project_work() {
    let (server, organization, owner, project) = setup().await;
    let client = &server.cells().unwrap();
    let repository = Uuid::now_v7();
    link(client, organization, owner, project, 1, repository).await;
    let record = work(repository);
    write::<Change>(
        client,
        organization,
        request(
            owner,
            Command::Item {
                project,
                version: 0,
                repository: record.repository,
                item: record.item,
            },
        ),
    )
    .await
    .unwrap();
    let result = write::<Change>(
        client,
        organization,
        request(
            owner,
            Command::Repository {
                project,
                version: 3,
                repository,
                linked: false,
            },
        ),
    )
    .await
    .unwrap();
    assert!(matches!(result, Outcome::Rejected { .. }));
    assert_eq!(items(client, organization, owner, project).await.total, 1);
    fixtures::close(&server).await;
}
#[tokio::test]
async fn stale_planning_edits_preserve_the_saved_project() {
    let (server, organization, owner, project) = setup().await;
    let client = &server.cells().unwrap();
    link(client, organization, owner, project, 1, Uuid::now_v7()).await;
    let result = write::<Change>(
        client,
        organization,
        request(
            owner,
            Command::Planning {
                project,
                version: 1,
                properties: Properties {
                    status: Status::Completed,
                    health: Health::OnTrack,
                    lead: None,
                    start: None,
                    target: None,
                },
            },
        ),
    )
    .await
    .unwrap();
    assert!(matches!(result, Outcome::Rejected { .. }));
    assert_eq!(
        items(client, organization, owner, project)
            .await
            .view
            .project
            .properties
            .status,
        Status::Planned
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn different_members_can_edit_distinct_items_from_the_same_collection() {
    let (server, organization, owner, project) = setup().await;
    let client = &server.cells().unwrap();
    let colleague = Uuid::now_v7();
    join(client, organization, owner, colleague, 0).await;
    write::<ChangeMember>(
        client,
        organization,
        MemberChange {
            actor: owner,
            member: colleague,
            version: 1,
            role: Some(Role::Owner),
        },
    )
    .await
    .unwrap();
    let mut drafts = [work(Uuid::now_v7()), work(Uuid::now_v7())];
    for (index, record) in drafts.iter_mut().enumerate() {
        record.repository = None;
        record.item.subject = None;
        record.item.title = format!("Deliverable {index}");
        let result = write::<Change>(
            client,
            organization,
            request(
                owner,
                Command::Item {
                    project,
                    version: 0,
                    repository: None,
                    item: record.item.clone(),
                },
            ),
        )
        .await
        .unwrap();
        assert!(matches!(result, Outcome::Applied { .. }));
    }
    drafts[0].item.priority = 1;
    drafts[1].item.priority = 2;
    let (first, second) = tokio::join!(
        write::<Change>(
            client,
            organization,
            request(
                owner,
                Command::Item {
                    project,
                    version: 1,
                    repository: None,
                    item: drafts[0].item.clone(),
                }
            )
        ),
        write::<Change>(
            client,
            organization,
            request(
                colleague,
                Command::Item {
                    project,
                    version: 1,
                    repository: None,
                    item: drafts[1].item.clone(),
                }
            )
        ),
    );
    assert!(matches!(
        (first.unwrap(), second.unwrap()),
        (Outcome::Applied { .. }, Outcome::Applied { .. })
    ));
    let saved = items(client, organization, owner, project).await;
    for record in drafts {
        assert_eq!(
            saved
                .items
                .iter()
                .find(|row| row.record.item.id == record.item.id)
                .unwrap()
                .record
                .item,
            record.item
        );
    }
    fixtures::close(&server).await;
}
#[tokio::test]
async fn outsiders_cannot_read_projects_or_allocate_submissions() {
    let (server, organization, _, project) = setup().await;
    let client = &server.cells().unwrap();
    let outsider = Uuid::now_v7();
    let result = client
        .query::<Get>(
            Address::Organization(organization),
            None,
            ProjectKey {
                actor: outsider,
                project,
            },
        )
        .await;
    assert!(matches!(result, Err(Error::AccessDenied)));
    let mut input = request(
        outsider,
        Command::Repository {
            project,
            version: 1,
            repository: Uuid::now_v7(),
            linked: true,
        },
    );
    input.cancel = true;
    assert!(matches!(
        write::<Change>(client, organization, input).await,
        Err(Error::AccessDenied)
    ));
    fixtures::close(&server).await;
}

async fn join(client: &CellClient, organization: Uuid, owner: Uuid, person: Uuid, version: u64) {
    let ChangeOutcome::Applied(offer) = write::<Invite>(
        client,
        organization,
        InviteInput {
            actor: owner,
            recipient: person,
            version,
            role: Role::Member,
        },
    )
    .await
    .unwrap() else {
        panic!("invitation failed");
    };
    let result = write::<DecideInvitation>(
        client,
        organization,
        Decision {
            actor: person,
            version: offer.version,
            accept: true,
        },
    )
    .await
    .unwrap();
    assert!(matches!(result, ChangeOutcome::Applied(_)));
}
#[tokio::test(flavor = "multi_thread")]
async fn rejoining_does_not_restore_project_management_but_original_decisions_recover() {
    let (server, organization, owner, _) = setup().await;
    let client = &server.cells().unwrap();
    let creator = Uuid::now_v7();
    join(client, organization, owner, creator, 0).await;
    let project = Uuid::now_v7();
    let original = request(
        creator,
        Command::Create {
            id: project,
            name: "Member project".into(),
            template: "board".into(),
        },
    );
    let expected = write::<Change>(client, organization, original.clone())
        .await
        .unwrap();
    write::<ChangeMember>(
        client,
        organization,
        MemberChange {
            actor: owner,
            member: creator,
            version: 1,
            role: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        write::<Change>(client, organization, original)
            .await
            .unwrap(),
        expected
    );
    join(client, organization, owner, creator, 2).await;
    let current = client
        .query::<Get>(
            Address::Organization(organization),
            None,
            ProjectKey {
                actor: creator,
                project,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert!(!current.can_manage);
    fixtures::close(&server).await;
}
pub(in crate::cells::organization) struct WithoutProjects;
impl Mutation for WithoutProjects {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 65502;
    type Input = Caller;
    type Output = bool;
    fn execute(context: &mut Transaction<'_>, _: Caller) -> Result<MutationResult<bool>> {
        context.delete_document(READY)?;
        context.delete_document("planning/v2/state")?;
        for table in [
            "planning_history",
            "planning_item_revisions",
            "organization_project_items",
            "organization_project_repositories",
            "organization_projects",
        ] {
            context.query(&format!("DROP TABLE {table}"), &[])?;
        }
        Ok(MutationResult::Success(true))
    }
}
#[tokio::test]
async fn existing_organizations_require_explicit_idempotent_preparation() {
    let (server, organization, owner, project) = setup().await;
    let client = &server.cells().unwrap();
    write::<WithoutProjects>(client, organization, Caller { actor: owner })
        .await
        .unwrap();
    let missing = client
        .query::<Get>(
            Address::Organization(organization),
            None,
            ProjectKey {
                actor: owner,
                project,
            },
        )
        .await;
    assert!(matches!(missing, Err(Error::SchemaUnavailable)));
    write::<Prepare>(
        client,
        organization,
        PrepareInput {
            actor: owner,
            version: 0,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        write::<Prepare>(
            client,
            organization,
            PrepareInput {
                actor: owner,
                version: 0
            }
        )
        .await
        .unwrap(),
        crate::LifecycleStatus::Ready
    );
    let record = client
        .query::<super::super::GetOrganization>(
            Address::Organization(organization),
            None,
            Caller { actor: owner },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(record.organization.name, "Engineering");
    fixtures::close(&server).await;
}
