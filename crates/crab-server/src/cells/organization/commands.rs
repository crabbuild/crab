use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BootstrapInput {
    pub actor: Uuid,
    pub handle: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub website: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub avatar: Option<crate::cells::directory::avatar::AvatarConfig>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditInput {
    pub actor: Uuid,
    pub version: u64,
    pub handle: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub website: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub avatar: Option<crate::cells::directory::avatar::AvatarConfig>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberChange {
    pub actor: Uuid,
    pub member: Uuid,
    pub version: u64,
    pub role: Option<Role>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum ChangeOutcome<T> {
    Applied(T),
    Conflict,
    LastOwner,
    NotFound,
}

pub(crate) struct Bootstrap;
impl Mutation for Bootstrap {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 1;
    type Input = BootstrapInput;
    type Output = ChangeOutcome<Organization>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        actor(input.actor)?;
        validate_profile(
            &input.handle,
            &input.name,
            &input.description,
            &input.website,
            &input.email,
            &input.location,
            input.avatar.as_ref(),
        )
        .map_err(Error::Invalid)?;
        let digest = blake3::hash(&serde_json::to_vec(&input)?);
        let old = context.query(
            "SELECT actor, digest FROM organization_bootstrap WHERE singleton = 1",
            &[],
        )?;
        if let Some(row) = old.rows.first() {
            let [owner, SqlValue::Blob(old_digest)] = row.as_slice() else {
                return Err(Error::Invalid("organization bootstrap record is invalid"));
            };
            if uuid(owner)? != input.actor || old_digest.as_slice() != digest.as_bytes() {
                return Ok(MutationResult::Rejected(ChangeOutcome::Conflict));
            }
            let value =
                record(context)?.ok_or(Error::Invalid("organization bootstrap has no record"))?;
            return Ok(MutationResult::Success(ChangeOutcome::Applied(value)));
        }
        teams::initialize(context)?;
        super::projects::initialize(context)?;
        context.query("INSERT INTO organization_details(singleton, name, description, created_at_ms, updated_at_ms) VALUES (1, ?, ?, ?, ?)", &[SqlValue::Text(input.name), SqlValue::Text(input.description), SqlValue::Integer(context.now_ms()), SqlValue::Integer(context.now_ms())])?;
        context.put_document(
            PROFILE_DOC_KEY,
            &OrganizationProfileExtras {
                handle: input.handle,
                website: input.website,
                email: input.email,
                location: input.location,
                avatar: input.avatar,
            },
        )?;
        context.query("INSERT INTO organization_members(actor, role, active, version) VALUES (?, 'owner', 1, 1)", &[blob(input.actor)])?;
        context.query(
            "INSERT INTO organization_bootstrap(singleton, actor, digest) VALUES (1, ?, ?)",
            &[
                blob(input.actor),
                SqlValue::Blob(digest.as_bytes().to_vec()),
            ],
        )?;
        teams::membership_changed(
            context,
            &Member {
                actor: input.actor,
                role: Role::Owner,
                active: true,
                version: 1,
            },
            None,
        )?;
        advance(context, input.actor, Action::Created)?;
        let value =
            record(context)?.ok_or(Error::Invalid("organization creation has no record"))?;
        Ok(MutationResult::Success(ChangeOutcome::Applied(value)))
    }
}
pub(crate) struct Edit;
impl Mutation for Edit {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 2;
    type Input = EditInput;
    type Output = ChangeOutcome<Organization>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        validate_profile(
            &input.handle,
            &input.name,
            &input.description,
            &input.website,
            &input.email,
            &input.location,
            input.avatar.as_ref(),
        )
        .map_err(Error::Invalid)?;
        let Some(mut current) = record(context)? else {
            return Ok(MutationResult::Rejected(ChangeOutcome::NotFound));
        };
        if current.version != input.version {
            return Ok(MutationResult::Rejected(ChangeOutcome::Conflict));
        }
        let handle_changed = current.handle != input.handle;
        let name_changed = current.name != input.name;
        let description_changed = current.description != input.description;
        let profile_changed = current.website != input.website
            || current.email != input.email
            || current.location != input.location
            || current.avatar != input.avatar;
        if handle_changed || name_changed || description_changed || profile_changed {
            // Wall-clock rollback cannot make valid persisted metadata unreadable.
            let now = context.now_ms().max(current.updated_at_ms as i64);
            let action = Action::Edited {
                name: name_changed,
                description: description_changed,
                handle: handle_changed,
                profile: profile_changed,
            };
            context.query("UPDATE organization_details SET name = ?, description = ?, updated_at_ms = ? WHERE singleton = 1", &[SqlValue::Text(input.name), SqlValue::Text(input.description), SqlValue::Integer(now)])?;
            context.put_document(
                PROFILE_DOC_KEY,
                &OrganizationProfileExtras {
                    handle: input.handle,
                    website: input.website,
                    email: input.email,
                    location: input.location,
                    avatar: input.avatar,
                },
            )?;
            advance(context, input.actor, action)?;
            current =
                record(context)?.ok_or(Error::Invalid("organization edit lost its record"))?;
        }
        Ok(MutationResult::Success(ChangeOutcome::Applied(current)))
    }
}
pub(crate) struct ChangeMember;
impl Mutation for ChangeMember {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 3;
    type Input = MemberChange;
    type Output = ChangeOutcome<Member>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        change_member(context, input)
    }
}

pub(crate) const MAX_DEPARTURE_MEMBERS: usize = 50;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DepartureMember {
    pub(crate) actor: Uuid,
    pub(crate) version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Departure {
    /// The requester: an owner, or a member removing only themselves.
    pub(crate) owner: Uuid,
    pub(crate) members: Vec<DepartureMember>,
    pub(crate) mode: DepartureMode,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DepartureMode {
    Remove,
    Convert,
}

impl Departure {
    /// Leaving removes only the requester, so membership suffices.
    pub(crate) fn is_self_removal(&self) -> bool {
        self.mode == DepartureMode::Remove
            && matches!(self.members.as_slice(), [member] if member.actor == self.owner)
    }

    pub(crate) fn valid(&self) -> bool {
        let distinct: std::collections::BTreeSet<_> =
            self.members.iter().map(|member| member.actor).collect();
        !self.owner.is_nil()
            && (1..=MAX_DEPARTURE_MEMBERS).contains(&self.members.len())
            && distinct.len() == self.members.len()
            && !distinct.contains(&Uuid::nil())
            && self
                .members
                .iter()
                .all(|member| (1..MAX_VERSION).contains(&member.version))
    }
}

fn selected_members(
    context: &Transaction<'_>,
    input: &Departure,
) -> Result<ChangeOutcome<Vec<Member>>> {
    if !input.valid() {
        return Err(Error::Invalid(
            "Choose 1 to 50 distinct members with current versions",
        ));
    }
    teams::ensure_ready(context)?;
    let mut selected = Vec::with_capacity(input.members.len());
    for requested in &input.members {
        let Some(saved) = member(context, requested.actor)? else {
            return Ok(ChangeOutcome::NotFound);
        };
        selected.push(saved);
    }
    Ok(ChangeOutcome::Applied(selected))
}

fn check_departure(
    context: &Transaction<'_>,
    input: &Departure,
    selected: &[Member],
) -> Result<ChangeOutcome<()>> {
    if !member(context, input.owner)?
        .is_some_and(|entry| entry.active && (entry.role == Role::Owner || input.is_self_removal()))
    {
        return Err(Error::AccessDenied);
    }
    if selected.iter().any(|member| !member.active) {
        return Ok(ChangeOutcome::NotFound);
    }
    if !selected
        .iter()
        .zip(&input.members)
        .all(|(saved, expected)| saved.version == expected.version)
    {
        return Ok(ChangeOutcome::Conflict);
    }
    // The selection is bounded to 50. A page of 51 owners always has
    // enough information to find a survivor even in a larger organization.
    let owners = context.query(
        "SELECT actor FROM organization_members WHERE role = 'owner' AND active = 1 ORDER BY actor LIMIT ?",
        &[SqlValue::Integer((MAX_DEPARTURE_MEMBERS + 1) as i64)],
    )?;
    for row in &owners.rows {
        let [owner] = row.as_slice() else {
            return Err(Error::Invalid("Invalid organization owner record"));
        };
        let owner = uuid(owner)?;
        if !selected.iter().any(|member| member.actor == owner) {
            return Ok(ChangeOutcome::Applied(()));
        }
    }
    Ok(ChangeOutcome::LastOwner)
}

pub(crate) struct CheckDeparture;
impl Read for CheckDeparture {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 97;
    type Input = Departure;
    type Output = ChangeOutcome<()>;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        match selected_members(context, &input)? {
            ChangeOutcome::Applied(selected) => check_departure(context, &input, &selected),
            _ => Ok(ChangeOutcome::NotFound),
        }
    }
}

pub(crate) struct FinalizeDeparture;
impl Mutation for FinalizeDeparture {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 97;
    type Input = Departure;
    type Output = ChangeOutcome<Vec<Member>>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        let ChangeOutcome::Applied(selected) = selected_members(context, &input)? else {
            return Ok(MutationResult::Rejected(ChangeOutcome::NotFound));
        };
        // Membership writers settle the journal first. An exact inactive
        // generation for every target proves a replay of this whole batch.
        if selected
            .iter()
            .zip(&input.members)
            .all(|(saved, expected)| {
                !saved.active && saved.version.checked_sub(1) == Some(expected.version)
            })
        {
            return Ok(MutationResult::Success(ChangeOutcome::Applied(selected)));
        }
        match check_departure(context, &input, &selected)? {
            ChangeOutcome::Applied(()) => {}
            ChangeOutcome::Conflict => {
                return Ok(MutationResult::Rejected(ChangeOutcome::Conflict));
            }
            ChangeOutcome::NotFound => {
                return Ok(MutationResult::Rejected(ChangeOutcome::NotFound));
            }
            ChangeOutcome::LastOwner => {
                return Ok(MutationResult::Rejected(ChangeOutcome::LastOwner));
            }
        }

        // Apply only after validating every member and the surviving owners.
        // A Cell transaction commits the entire membership and audit batch.
        let members = selected
            .into_iter()
            .map(|member| {
                apply_member(
                    context,
                    input.owner,
                    member,
                    None,
                    input.mode == DepartureMode::Convert,
                )
            })
            .collect::<Result<_>>()?;
        Ok(MutationResult::Success(ChangeOutcome::Applied(members)))
    }
}

fn change_member(
    context: &mut Transaction<'_>,
    input: MemberChange,
) -> Result<MutationResult<ChangeOutcome<Member>>> {
    teams::ensure_ready(context)?;
    actor(input.member)?;
    let Some(old) = member(context, input.member)?.filter(|member| member.active) else {
        return Ok(MutationResult::Rejected(ChangeOutcome::NotFound));
    };
    if old.version != input.version {
        return Ok(MutationResult::Rejected(ChangeOutcome::Conflict));
    }
    let role = input.role.unwrap_or(old.role);
    let active = input.role.is_some();
    if old.active == active && old.role == role {
        return Ok(MutationResult::Success(ChangeOutcome::Applied(old)));
    }
    if old.role == Role::Owner && (!active || role != Role::Owner) {
        let others = context.query("SELECT 1 FROM organization_members WHERE role = 'owner' AND active = 1 AND actor <> ? LIMIT 1", &[blob(input.member)])?;
        if others.rows.is_empty() {
            return Ok(MutationResult::Rejected(ChangeOutcome::LastOwner));
        }
    }
    let changed = apply_member(context, input.actor, old, input.role, false)?;
    Ok(MutationResult::Success(ChangeOutcome::Applied(changed)))
}

fn apply_member(
    context: &mut Transaction<'_>,
    actor: Uuid,
    old: Member,
    next_role: Option<Role>,
    converting: bool,
) -> Result<Member> {
    let role = next_role.unwrap_or(old.role);
    let active = next_role.is_some();
    let version = old
        .version
        .checked_add(1)
        .filter(|version| *version <= MAX_VERSION)
        .ok_or(Error::Invalid(
            "organization membership version limit reached",
        ))?;
    // Removal retains the version that invitation acceptance must advance.
    // An edit captured before removal cannot replace the later grant.
    let cancelled_invitations = if active {
        0
    } else {
        invitations::remember_removal(context, old.actor, version)?;
        invitations::cancel_issued(context, old.actor)?
    };
    context.query(
        "UPDATE organization_members SET role = ?, active = ?, version = ? WHERE actor = ?",
        &[
            SqlValue::Text(role.name().into()),
            SqlValue::Integer(i64::from(active)),
            integer(version)?,
            blob(old.actor),
        ],
    )?;
    teams::membership_changed(
        context,
        &Member {
            actor: old.actor,
            role,
            active,
            version,
        },
        Some(old.active),
    )?;
    advance(
        context,
        actor,
        if converting {
            Action::MemberConverted {
                member: old.actor,
                version,
                cancelled_invitations,
            }
        } else {
            Action::Membership {
                member: old.actor,
                role: next_role,
                version,
                cancelled_invitations,
            }
        },
    )?;
    Ok(Member {
        actor: old.actor,
        role,
        active,
        version,
    })
}
