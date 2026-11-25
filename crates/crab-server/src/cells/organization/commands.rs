use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BootstrapInput {
    pub actor: Uuid,
    pub name: String,
    pub description: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditInput {
    pub actor: Uuid,
    pub version: u64,
    pub name: String,
    pub description: String,
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
        details(&input.name, &input.description)?;
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
        details(&input.name, &input.description)?;
        let Some(mut current) = record(context)? else {
            return Ok(MutationResult::Rejected(ChangeOutcome::NotFound));
        };
        if current.version != input.version {
            return Ok(MutationResult::Rejected(ChangeOutcome::Conflict));
        }
        if current.name != input.name || current.description != input.description {
            // Wall-clock rollback cannot make valid persisted metadata unreadable.
            let now = context.now_ms().max(current.updated_at_ms as i64);
            let action = Action::Edited {
                name: current.name != input.name,
                description: current.description != input.description,
            };
            context.query("UPDATE organization_details SET name = ?, description = ?, updated_at_ms = ? WHERE singleton = 1", &[SqlValue::Text(input.name), SqlValue::Text(input.description), SqlValue::Integer(now)])?;
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

pub(crate) const MAX_CONVERSION_MEMBERS: usize = 50;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConversionMember {
    pub(crate) actor: Uuid,
    pub(crate) version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Conversion {
    pub(crate) owner: Uuid,
    pub(crate) members: Vec<ConversionMember>,
}

impl Conversion {
    pub(crate) fn valid(&self) -> bool {
        let distinct: std::collections::BTreeSet<_> =
            self.members.iter().map(|member| member.actor).collect();
        !self.owner.is_nil()
            && (1..=MAX_CONVERSION_MEMBERS).contains(&self.members.len())
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
    input: &Conversion,
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

fn check_conversion(
    context: &Transaction<'_>,
    input: &Conversion,
    selected: &[Member],
) -> Result<ChangeOutcome<()>> {
    if !member(context, input.owner)?.is_some_and(|entry| entry.active && entry.role == Role::Owner)
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
        &[SqlValue::Integer((MAX_CONVERSION_MEMBERS + 1) as i64)],
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

pub(crate) struct CheckConversion;
impl Read for CheckConversion {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 92;
    type Input = Conversion;
    type Output = ChangeOutcome<()>;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        match selected_members(context, &input)? {
            ChangeOutcome::Applied(selected) => check_conversion(context, &input, &selected),
            _ => Ok(ChangeOutcome::NotFound),
        }
    }
}

pub(crate) struct FinalizeConversion;
impl Mutation for FinalizeConversion {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 92;
    type Input = Conversion;
    type Output = ChangeOutcome<()>;

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
            return Ok(MutationResult::Success(ChangeOutcome::Applied(())));
        }
        let guard = check_conversion(context, &input, &selected)?;
        if guard != ChangeOutcome::Applied(()) {
            return Ok(MutationResult::Rejected(guard));
        }

        // Apply only after validating every member and the surviving owners.
        // A Cell transaction commits the entire membership and audit batch.
        for member in selected {
            apply_member(context, input.owner, member, None, true)?;
        }
        Ok(MutationResult::Success(ChangeOutcome::Applied(())))
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
            }
        } else {
            Action::Membership {
                member: old.actor,
                role: next_role,
                version,
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
