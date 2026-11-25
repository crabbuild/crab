use super::*;

pub(in crate::cells::organization) fn handles(operation: u32, mutation: bool) -> bool {
    if mutation {
        matches!(operation, Prepare::ID | Change::ID | Cancel::ID)
    } else {
        matches!(
            operation,
            GetPreparation::ID
                | List::ID
                | Get::ID
                | sync::GetSync::ID
                | Members::ID
                | GetMembership::ID
                | Candidates::ID
                | review_routing::GetSettings::ID
                | GetPolicy::ID
                | MentionMembers::ID
                | ResolveReviewTeams::ID
                | hierarchy_requests::List::ID
        )
    }
}
pub(in crate::cells::organization) fn authorize(
    context: &Transaction<'_>,
    operation: u32,
    mutation: bool,
    input: &serde_json::Value,
) -> Result<()> {
    #[derive(Deserialize)]
    struct Person {
        actor: Uuid,
    }
    let who: Person = serde_json::from_value(input.clone())?;
    actor(who.actor)?;
    let membership = member(context, who.actor)?;
    if mutation && matches!(operation, Change::ID | Cancel::ID) {
        if membership.is_none() {
            return Err(Error::AccessDenied);
        }
        ensure_ready(context)?;
        let request: Request = serde_json::from_value(input.clone())?;
        actor(request.request_id)?;
        // Terminal decisions contain only a known UUID or rejection code.
        // Their actor may recover after revocation; protected team content and
        // grants cannot pass this exception, including SDK receipt replay.
        if let Some(outcome) = commands::recover(context, &request)?
            && !matches!(outcome, Outcome::SubmissionConflict)
        {
            return Ok(());
        }
    }
    if mutation && operation == Cancel::ID {
        // A current or former organization member can fence only their own
        // immutable submission. This returns no organization or team content.
        return Ok(());
    }
    let organization = membership
        .filter(|member| member.active)
        .ok_or(Error::AccessDenied)?;
    let owner = organization.role == OrganizationRole::Owner;
    if matches!(
        (operation, mutation),
        (GetPreparation::ID, false) | (Prepare::ID, true) | (GetPolicy::ID, false)
    ) {
        return if owner {
            Ok(())
        } else {
            Err(Error::AccessDenied)
        };
    }
    ensure_ready(context)?;
    if mutation {
        let request: Request = serde_json::from_value(input.clone())?;
        let allowed = match request.command {
            Command::Create { parent, .. } => {
                (owner || policy(context)?.member_creation)
                    && hierarchy::can_choose_parent(context, who.actor, owner, parent)?
            }
            Command::Policy { .. } => owner,
            Command::Reparent { team, parent, .. } => {
                (owner || view(context, who.actor, team)?.is_some_and(|view| view.can_manage))
                    && hierarchy::can_choose_parent(context, who.actor, owner, parent)?
            }
            Command::ProposeHierarchy {
                team, parent, side, ..
            } => {
                owner
                    || view(context, who.actor, side.requesting(team, parent))?
                        .is_some_and(|view| view.can_manage)
            }
            // Each decision checks its exact request and reviewing side inside
            // the mutation so a vanished or hidden request resolves uniformly.
            Command::DecideHierarchy { .. } => true,
            Command::Delete { team, .. } => {
                owner
                    || (view(context, who.actor, team)?.is_some_and(|view| view.can_manage)
                        && !hierarchy::has_children(context, team)?)
            }
            Command::Edit { team, .. }
            | Command::Member { team, .. }
            | Command::ReviewRouting { team, .. } => {
                owner || view(context, who.actor, team)?.is_some_and(|view| view.can_manage)
            }
        };
        return if allowed {
            Ok(())
        } else {
            Err(Error::AccessDenied)
        };
    }
    if matches!(
        operation,
        List::ID | ResolveReviewTeams::ID | hierarchy_requests::List::ID
    ) {
        return Ok(());
    }
    #[derive(Deserialize)]
    struct Target {
        team: Uuid,
    }
    let target: Target = serde_json::from_value(input.clone())?;
    let visible = view(context, who.actor, target.team)?.ok_or(Error::AccessDenied)?;
    if matches!(
        operation,
        GetMembership::ID | Candidates::ID | review_routing::GetSettings::ID
    ) && !visible.can_manage
    {
        return Err(Error::AccessDenied);
    }
    Ok(())
}
