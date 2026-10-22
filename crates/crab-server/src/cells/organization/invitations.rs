use super::*;

const LIFETIME_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InvitationState {
    Pending,
    Accepted,
    Declined,
    Cancelled,
    Expired,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Invitation {
    pub recipient: Uuid,
    pub role: Role,
    pub version: u64,
    pub member_version: u64,
    pub state: InvitationState,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}
impl Invitation {
    fn pending_at(&self, now: i64) -> bool {
        self.state == InvitationState::Pending && now < self.expires_at_ms as i64
    }
    fn visible(mut self, now: i64) -> Self {
        if self.state == InvitationState::Pending && !self.pending_at(now) {
            self.state = InvitationState::Expired;
        }
        self
    }
    fn action(&self) -> Action {
        Action::Invitation {
            recipient: self.recipient,
            role: self.role,
            version: self.version,
            state: self.state,
        }
    }
}
fn row(row: &[SqlValue]) -> Result<Invitation> {
    let [
        recipient,
        role,
        version,
        member_version,
        state,
        created,
        expires,
    ] = row
    else {
        return Err(Error::Invalid("organization invitation record is invalid"));
    };
    let state = match text(state)?.as_str() {
        "pending" => InvitationState::Pending,
        "accepted" => InvitationState::Accepted,
        "declined" => InvitationState::Declined,
        "cancelled" => InvitationState::Cancelled,
        _ => return Err(Error::Invalid("organization invitation state is invalid")),
    };
    Ok(Invitation {
        recipient: uuid(recipient)?,
        role: Role::parse(&text(role)?)?,
        version: uint(version)?,
        member_version: uint(member_version)?,
        state,
        created_at_ms: uint(created)?,
        expires_at_ms: uint(expires)?,
    })
}
const COLUMNS: &str =
    "recipient, role, version, member_version, state, created_at_ms, expires_at_ms";
fn invitation(context: &Transaction<'_>, recipient: Uuid) -> Result<Option<Invitation>> {
    actor(recipient)?;
    context
        .query(
            &format!("SELECT {COLUMNS} FROM organization_invitations WHERE recipient = ?"),
            &[blob(recipient)],
        )?
        .rows
        .first()
        .map(|values| row(values))
        .transpose()
}
fn next_version(version: u64) -> Result<u64> {
    version
        .checked_add(1)
        .filter(|value| *value <= MAX_VERSION)
        .ok_or(Error::Invalid(
            "organization invitation version limit reached",
        ))
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InviteInput {
    pub actor: Uuid,
    pub recipient: Uuid,
    pub version: u64,
    pub role: Role,
}
pub(crate) struct Invite;
impl Mutation for Invite {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 4;
    type Input = InviteInput;
    type Output = ChangeOutcome<Invitation>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        let old = invitation(context, input.recipient)?;
        if old.as_ref().map_or(0, |value| value.version) != input.version {
            return Ok(MutationResult::Rejected(ChangeOutcome::Conflict));
        }
        let member = member(context, input.recipient)?;
        if member.as_ref().is_some_and(|member| member.active) {
            return Ok(MutationResult::Rejected(ChangeOutcome::Conflict));
        }
        let now = context.now_ms();
        let pending = old.filter(|value| value.pending_at(now));
        if let Some(old) = &pending
            && old.role == input.role
        {
            return Ok(MutationResult::Success(ChangeOutcome::Applied(old.clone())));
        }
        // Editing a pending offer keeps its original deadline. Retrying an expired
        // or closed offer creates a new version; old acceptance links cannot grant.
        let created = pending
            .as_ref()
            .map_or(now, |value| value.created_at_ms as i64);
        let expires = if let Some(value) = &pending {
            value.expires_at_ms as i64
        } else {
            now.checked_add(LIFETIME_MS)
                .ok_or(Error::Invalid("organization clock limit reached"))?
        };
        let value = Invitation {
            recipient: input.recipient,
            role: input.role,
            version: next_version(input.version)?,
            member_version: member.map_or(0, |member| member.version),
            state: InvitationState::Pending,
            created_at_ms: u64::try_from(created)
                .map_err(|_| Error::Invalid("organization clock is negative"))?,
            expires_at_ms: u64::try_from(expires)
                .map_err(|_| Error::Invalid("organization clock is negative"))?,
        };
        context.query("INSERT INTO organization_invitations(recipient, role, version, member_version, state, created_at_ms, expires_at_ms) VALUES (?, ?, ?, ?, 'pending', ?, ?) ON CONFLICT(recipient) DO UPDATE SET role = excluded.role, version = excluded.version, member_version = excluded.member_version, state = excluded.state, created_at_ms = excluded.created_at_ms, expires_at_ms = excluded.expires_at_ms", &[
            blob(value.recipient), SqlValue::Text(value.role.name().into()), integer(value.version)?, integer(value.member_version)?, integer(value.created_at_ms)?, integer(value.expires_at_ms)?
        ])?;
        advance(context, input.actor, value.action())?;
        Ok(MutationResult::Success(ChangeOutcome::Applied(value)))
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InvitationKey {
    pub actor: Uuid,
    pub recipient: Uuid,
    pub version: u64,
}
pub(crate) struct CancelInvitation;
impl Mutation for CancelInvitation {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 5;
    type Input = InvitationKey;
    type Output = ChangeOutcome<Invitation>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        let Some(mut value) = invitation(context, input.recipient)? else {
            return Ok(MutationResult::Rejected(ChangeOutcome::NotFound));
        };
        if value.version != input.version || value.state != InvitationState::Pending {
            return Ok(MutationResult::Rejected(ChangeOutcome::Conflict));
        }
        value.version = next_version(value.version)?;
        value.state = InvitationState::Cancelled;
        context.query("UPDATE organization_invitations SET state = 'cancelled', version = ? WHERE recipient = ?", &[integer(value.version)?, blob(value.recipient)])?;
        advance(context, input.actor, value.action())?;
        Ok(MutationResult::Success(ChangeOutcome::Applied(value)))
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Decision {
    pub actor: Uuid,
    pub version: u64,
    pub accept: bool,
}
pub(crate) struct DecideInvitation;
impl Mutation for DecideInvitation {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 6;
    type Input = Decision;
    type Output = ChangeOutcome<Invitation>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        let Some(mut value) = invitation(context, input.actor)? else {
            return Ok(MutationResult::Rejected(ChangeOutcome::NotFound));
        };
        if value.version != input.version || !value.pending_at(context.now_ms()) {
            return Ok(MutationResult::Rejected(ChangeOutcome::Conflict));
        }
        if input.accept {
            teams::ensure_ready(context)?;
            let old = member(context, input.actor)?;
            if old.as_ref().is_some_and(|member| member.active)
                || old.map_or(0, |member| member.version) != value.member_version
            {
                return Ok(MutationResult::Rejected(ChangeOutcome::Conflict));
            }
            let version = next_version(value.member_version)?;
            // The offer is an organization-owned capability until cancelled or
            // expired. Acceptance and the exact membership version commit together.
            context.query("INSERT INTO organization_members(actor, role, active, version) VALUES (?, ?, 1, ?) ON CONFLICT(actor) DO UPDATE SET role = excluded.role, active = 1, version = excluded.version", &[blob(input.actor), SqlValue::Text(value.role.name().into()), integer(version)?])?;
            teams::membership_changed(
                context,
                &Member {
                    actor: input.actor,
                    role: value.role,
                    active: true,
                    version,
                },
                Some(false),
            )?;
        }
        value.version = next_version(value.version)?;
        value.state = if input.accept {
            InvitationState::Accepted
        } else {
            InvitationState::Declined
        };
        let state = if input.accept { "accepted" } else { "declined" };
        context.query(
            "UPDATE organization_invitations SET state = ?, version = ? WHERE recipient = ?",
            &[
                SqlValue::Text(state.into()),
                integer(value.version)?,
                blob(input.actor),
            ],
        )?;
        advance(context, input.actor, value.action())?;
        Ok(MutationResult::Success(ChangeOutcome::Applied(value)))
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct InvitationView {
    pub organization: Option<Organization>,
    pub invitation: Option<Invitation>,
}
pub(crate) struct GetInvitation;
impl Read for GetInvitation {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 5;
    type Input = MemberKey;
    type Output = Option<InvitationView>;
    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        let invitation = invitation(context, input.member)?;
        // A recipient with no offer must not discover an unrelated private org.
        if invitation.is_none() && input.actor == input.member {
            return Ok(None);
        }
        // A closed offer remains visible to its recipient as history, but must
        // not reveal current organization metadata after their access ends.
        let can_view = input.actor != input.member
            || member(context, input.actor)?.is_some_and(|member| member.active)
            || invitation
                .as_ref()
                .is_some_and(|value| value.pending_at(context.now_ms()));
        let organization = if can_view {
            Some(record(context)?.ok_or(Error::Invalid("organization record is missing"))?)
        } else {
            None
        };
        Ok(Some(InvitationView {
            organization,
            invitation: invitation.map(|value| value.visible(context.now_ms())),
        }))
    }
}
pub(crate) struct ListInvitations;
impl Read for ListInvitations {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 6;
    type Input = PageQuery;
    type Output = Page<Invitation>;
    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        if !(1..=50).contains(&input.limit) {
            return Err(Error::Invalid(
                "organization invitation pages require 1–50 entries",
            ));
        }
        let organization =
            record(context)?.ok_or(Error::Invalid("organization record is missing"))?;
        let mut values = Vec::new();
        let predicate = if let Some(cursor) = &input.after {
            actor(cursor.after)?;
            if cursor.collection != Collection::Invitations
                || cursor.organization != organization.id
                || cursor.revision != organization.version
            {
                return Ok(Page::Changed);
            }
            values.push(blob(cursor.after));
            " AND recipient > ?"
        } else {
            ""
        };
        values.push(SqlValue::Integer(i64::from(input.limit) + 1));
        let mut items = context.query(&format!("SELECT {COLUMNS} FROM organization_invitations WHERE state = 'pending'{predicate} ORDER BY recipient LIMIT ?"), &values)?.rows.iter().map(|values| row(values).map(|value| value.visible(context.now_ms()))).collect::<Result<Vec<_>>>()?;
        let more = items.len() > usize::from(input.limit);
        items.truncate(usize::from(input.limit));
        let next = if more {
            items.last().map(|value| PageCursor {
                collection: Collection::Invitations,
                organization: organization.id,
                revision: organization.version,
                after: value.recipient,
            })
        } else {
            None
        };
        Ok(Page::Page {
            items,
            next,
            revision: organization.version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_acceptance_deadline_and_read_state_use_the_same_exclusive_boundary() {
        let offer = Invitation {
            recipient: Uuid::now_v7(),
            role: Role::Member,
            version: 1,
            member_version: 0,
            state: InvitationState::Pending,
            created_at_ms: 1000,
            expires_at_ms: 1000 + LIFETIME_MS as u64,
        };
        for (offset, expected) in [
            (-1, InvitationState::Pending),
            (0, InvitationState::Expired),
            (1, InvitationState::Expired),
        ] {
            let now = offer.expires_at_ms as i64 + offset;
            assert_eq!(
                (offer.pending_at(now), offer.clone().visible(now).state),
                (offset < 0, expected)
            );
        }
    }
}
