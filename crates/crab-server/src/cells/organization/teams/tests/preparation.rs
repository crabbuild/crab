use super::*;

pub(in crate::cells::organization) struct WithoutTeams;
impl Mutation for WithoutTeams {
    const CLASS: &'static str = crate::cells::organization::CLASS;
    const ID: u32 = 65_500;
    type Input = Caller;
    type Output = ();
    fn execute(context: &mut Transaction<'_>, _: Caller) -> Result<MutationResult<()>> {
        context.install_schema("DROP TABLE organization_team_members; DROP TABLE organization_team_submissions; DROP TABLE organization_teams; DROP TABLE organization_team_settings; DROP TABLE organization_membership_epochs;")?;
        context.delete_document("organization/teams/v1/state")?;
        Ok(MutationResult::Success(()))
    }
}
pub(in crate::cells::organization) struct FailedStep;
impl Mutation for FailedStep {
    const CLASS: &'static str = crate::cells::organization::CLASS;
    const ID: u32 = 65_501;
    type Input = PrepareInput;
    type Output = ();
    fn execute(context: &mut Transaction<'_>, input: PrepareInput) -> Result<MutationResult<()>> {
        Prepare::execute(context, input)?;
        Err(Error::Invalid("preparation interrupted"))
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn explicit_preparation_is_bounded_atomic_and_preserves_membership_versions() {
    let f = fixture().await;
    let client = &f.server.cells().unwrap();
    for _ in 0..9 {
        join(client, f.organization, f.owner, Uuid::now_v7(), 0).await;
    }
    let before = read::<GetOrganization>(client, f.organization, Caller { actor: f.owner }).await;
    write::<WithoutTeams>(client, f.organization, Caller { actor: f.owner }).await;
    assert_eq!(
        read::<GetPreparation>(client, f.organization, Caller { actor: f.owner }).await,
        OrganizationTeamState::Missing
    );
    let denied = client
        .command::<ChangeMember>(
            Address::Organization(f.organization),
            crate::app::mutation_identity().unwrap(),
            MemberChange {
                actor: f.owner,
                member: f.bob,
                version: 1,
                role: Some(OrganizationRole::Owner),
            },
        )
        .await;
    assert!(matches!(denied.err().unwrap(), Error::SchemaUnavailable));
    assert!(matches!(
        client
            .command::<Cancel>(
                Address::Organization(f.organization),
                crate::app::mutation_identity().unwrap(),
                request(
                    Uuid::now_v7(),
                    Command::Create {
                        name: "Unrelated".into(),
                        description: String::new(),
                        visibility: Visibility::Secret
                    }
                )
            )
            .await
            .err()
            .unwrap(),
        Error::AccessDenied
    ));
    let initial = write::<Prepare>(
        client,
        f.organization,
        PrepareInput {
            actor: f.owner,
            step: Step::Begin,
        },
    )
    .await;
    assert!(matches!(
        initial,
        OrganizationTeamState::Building {
            processed: 0,
            version: 1,
            ..
        }
    ));
    assert!(
        client
            .command::<FailedStep>(
                Address::Organization(f.organization),
                crate::app::mutation_identity().unwrap(),
                PrepareInput {
                    actor: f.owner,
                    step: Step::Step { version: 1 }
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        read::<GetPreparation>(client, f.organization, Caller { actor: f.owner }).await,
        initial
    );
    let next = write::<Prepare>(
        client,
        f.organization,
        PrepareInput {
            actor: f.owner,
            step: Step::Step { version: 1 },
        },
    )
    .await;
    assert!(matches!(
        next,
        OrganizationTeamState::Building {
            processed: 8,
            version: 2,
            ..
        }
    ));
    assert_eq!(
        write::<Prepare>(
            client,
            f.organization,
            PrepareInput {
                actor: f.owner,
                step: Step::Step { version: 2 }
            }
        )
        .await,
        OrganizationTeamState::Ready
    );
    assert_eq!(
        read::<GetOrganization>(client, f.organization, Caller { actor: f.owner }).await,
        before
    );
    let id = create(&f, f.owner, "Prepared", Visibility::Secret).await;
    change_member(&f, f.owner, id, f.bob, 0, Some(Role::Member)).await;
    assert_eq!(own(&f, f.bob, id).await.role, Some(Role::Member));
    fixtures::close(&f.server).await;
}
