use super::reads::View;
use super::*;

#[derive(Deserialize, Serialize)]
pub(crate) enum Landing {
    Member {
        view: View,
        can_create: bool,
        default_branch: String,
    },
    Invitation {
        organization: Organization,
        invitation: Invitation,
    },
}

pub(crate) struct GetLanding;
impl Read for GetLanding {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 80;
    type Input = Caller;
    type Output = Option<Landing>;

    fn execute(context: &mut Transaction<'_>, input: Caller) -> Result<Self::Output> {
        actor(input.actor)?;
        if let Some(role) = member(context, input.actor)?
            .filter(|member| member.active)
            .map(|member| member.role)
        {
            let policy = repository_policy::load(context)?;
            let can_create = policy.can_create(role);
            return Ok(record(context)?.map(|organization| Landing::Member {
                view: View { organization, role },
                can_create,
                default_branch: policy.default_branch,
            }));
        }
        let view = GetInvitation::execute(
            context,
            MemberKey {
                actor: input.actor,
                member: input.actor,
            },
        )?;
        let Some(InvitationView {
            organization: Some(organization),
            invitation: Some(invitation),
            ..
        }) = view
        else {
            return Ok(None);
        };
        Ok(
            (invitation.state == invitations::InvitationState::Pending).then_some(
                Landing::Invitation {
                    organization,
                    invitation,
                },
            ),
        )
    }
}
