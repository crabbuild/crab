use super::*;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "TeamHierarchySide"))]
pub(crate) enum Side {
    Child,
    Parent,
}

impl Side {
    pub(super) fn requesting(self, child: Uuid, parent: Uuid) -> Uuid {
        match self {
            Self::Child => child,
            Self::Parent => parent,
        }
    }

    fn reviewing(self, child: Uuid, parent: Uuid) -> Uuid {
        self.requesting(parent, child)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Child => "child",
            Self::Parent => "parent",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "TeamHierarchyDecision"))]
pub(crate) enum Decision {
    Approve,
    Decline,
    Withdraw,
}

struct Pending {
    id: Uuid,
    child: Uuid,
    parent: Uuid,
    child_version: u64,
    requester: Uuid,
    requester_epoch: u64,
    side: Side,
}

fn pending_row(row: &[SqlValue]) -> Result<Pending> {
    let [
        id,
        child,
        parent,
        child_version,
        requester,
        requester_epoch,
        side,
    ] = row
    else {
        return Err(Error::Invalid("team hierarchy request is invalid"));
    };
    let side = match text(side)?.as_str() {
        "child" => Side::Child,
        "parent" => Side::Parent,
        _ => return Err(Error::Invalid("team hierarchy request side is invalid")),
    };
    Ok(Pending {
        id: uuid(id)?,
        child: uuid(child)?,
        parent: uuid(parent)?,
        child_version: uint(child_version)?,
        requester: uuid(requester)?,
        requester_epoch: uint(requester_epoch)?,
        side,
    })
}

fn requester_can_move(context: &Transaction<'_>, pending: &Pending) -> Result<bool> {
    if !member(context, pending.requester)?.is_some_and(|member| member.active)
        || membership_epoch(context, pending.requester)? != pending.requester_epoch
    {
        return Ok(false);
    }
    let team = pending.side.requesting(pending.child, pending.parent);
    Ok(view(context, pending.requester, team)?.is_some_and(|view| view.can_manage))
}

pub(super) fn discard_for_team(context: &Transaction<'_>, id: Uuid) -> Result<()> {
    context.query(
        "DELETE FROM organization_team_hierarchy_requests WHERE child=?1 OR parent=?1",
        &[blob(id)],
    )?;
    Ok(())
}

pub(super) fn propose(
    context: &Transaction<'_>,
    input: &Request,
    child: Uuid,
    parent: Uuid,
    version: u64,
    side: Side,
) -> Result<Outcome> {
    let Some(child_view) = view(context, input.actor, child)? else {
        return Ok(Outcome::Missing);
    };
    if view(context, input.actor, parent)?.is_none() {
        return Ok(Outcome::Missing);
    }
    let child_team = child_view.team;
    if child_team.version != version || child_team.parent == Some(parent) {
        return Ok(Outcome::Conflict);
    }
    if !hierarchy::valid_parent(context, child, child_team.visibility, Some(parent))? {
        return Ok(Outcome::InvalidParent);
    }
    let epoch = membership_epoch(context, input.actor)?;
    let pair = [
        blob(child),
        blob(parent),
        SqlValue::Text(side.name().into()),
    ];
    let rows = context.query(
        "SELECT id,child,parent,child_version,requester,requester_epoch,side
         FROM organization_team_hierarchy_requests WHERE child=?1 AND parent=?2 AND side=?3",
        &pair,
    )?;
    if let Some(row) = rows.rows.first() {
        let previous = pending_row(row)?;
        if previous.child_version == version && requester_can_move(context, &previous)? {
            return Ok(Outcome::Conflict);
        }
        context.query(
            "DELETE FROM organization_team_hierarchy_requests WHERE id=?",
            &[blob(previous.id)],
        )?;
    }
    context.query(
        "INSERT INTO organization_team_hierarchy_requests
            (child,parent,side,id,child_version,requester,requester_epoch) VALUES (?,?,?,?,?,?,?)",
        &[
            pair[0].clone(),
            pair[1].clone(),
            pair[2].clone(),
            blob(input.request_id),
            integer(version)?,
            blob(input.actor),
            integer(epoch)?,
        ],
    )?;
    Ok(Outcome::Applied { id: child })
}

pub(super) fn decide(
    context: &Transaction<'_>,
    who: Uuid,
    child: Uuid,
    id: Uuid,
    decision: Decision,
) -> Result<Outcome> {
    actor(id)?;
    let rows = context.query(
        "SELECT id,child,parent,child_version,requester,requester_epoch,side
         FROM organization_team_hierarchy_requests WHERE id=?1 AND child=?2",
        &[blob(id), blob(child)],
    )?;
    let Some(row) = rows.rows.first() else {
        return Ok(Outcome::Missing);
    };
    let pending = pending_row(row)?;
    let reviewer = pending.side.reviewing(child, pending.parent);
    let allowed = match decision {
        Decision::Withdraw => who == pending.requester,
        Decision::Approve | Decision::Decline => {
            who != pending.requester
                && view(context, who, reviewer)?.is_some_and(|view| view.can_manage)
        }
    };
    if !allowed {
        return Ok(Outcome::Missing);
    }
    if matches!(decision, Decision::Approve) {
        if !requester_can_move(context, &pending)? {
            return Ok(Outcome::Conflict);
        }
        // Apply through the same version and graph checks as a direct move.
        // The authority check and edge publication share this Cell transaction.
        return commands::reparent(
            context,
            who,
            child,
            pending.child_version,
            Some(pending.parent),
        );
    }
    context.query(
        "DELETE FROM organization_team_hierarchy_requests WHERE id=?",
        &[blob(id)],
    )?;
    Ok(Outcome::Applied { id: child })
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListInput {
    pub actor: Uuid,
    pub team: Option<Uuid>,
    pub after: Option<Uuid>,
    pub request: Option<Uuid>,
    pub incoming: bool,
    pub limit: u8,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "TeamHierarchyRequest"))]
pub(crate) struct Item {
    pub id: Uuid,
    pub child: Uuid,
    pub child_name: String,
    pub parent: Uuid,
    pub parent_name: String,
    pub side: Side,
    pub can_review: bool,
    pub can_approve: bool,
    pub can_withdraw: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "TeamHierarchyRequests"))]
pub(crate) struct Page {
    pub items: Vec<Item>,
    pub next: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub organization_name: Option<String>,
}

pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 62;
    type Input = ListInput;
    type Output = Page;

    fn execute(context: &mut Transaction<'_>, input: ListInput) -> Result<Page> {
        if !(1..=50).contains(&input.limit) {
            return Err(Error::Invalid("team request pages require 1–50 entries"));
        }
        let organization = member(context, input.actor)?
            .filter(|member| member.active)
            .ok_or(Error::AccessDenied)?;
        let owner = organization.role == OrganizationRole::Owner;
        let rows = context.query(
            "SELECT r.id,r.child,r.parent,r.child_version,r.requester,r.requester_epoch,r.side,
                    child.name,parent.name,child.version,
                    ?2 OR COALESCE(m.role='maintainer' AND m.membership_epoch=e.epoch,0)
             FROM organization_team_hierarchy_requests r
             JOIN organization_teams child ON child.id=r.child
             JOIN organization_teams parent ON parent.id=r.parent
             LEFT JOIN organization_team_members m ON m.actor=?1
               AND m.team=CASE r.side WHEN 'child' THEN r.parent ELSE r.child END
             LEFT JOIN organization_membership_epochs e ON e.actor=m.actor
             WHERE (?2 OR r.requester=?1 OR (m.role='maintainer' AND m.membership_epoch=e.epoch))
               AND (?3 IS NULL OR r.child=?3 OR r.parent=?3)
               AND (?4 IS NULL OR r.id>?4)
               AND (?6 IS NULL OR r.id=?6)
               AND (NOT ?7 OR r.requester<>?1)
               AND child.deleted_at_ms IS NULL AND parent.deleted_at_ms IS NULL
               AND child.visibility='closed' AND parent.visibility='closed'
             ORDER BY r.id LIMIT ?5",
            &[
                blob(input.actor),
                SqlValue::Integer(i64::from(owner)),
                input.team.map_or(SqlValue::Null, blob),
                input.after.map_or(SqlValue::Null, blob),
                SqlValue::Integer(i64::from(input.limit) + 1),
                input.request.map_or(SqlValue::Null, blob),
                SqlValue::Integer(i64::from(input.incoming)),
            ],
        )?;
        let limit = usize::from(input.limit);
        let items = rows
            .rows
            .iter()
            .take(limit)
            .map(|row| {
                let [pending @ .., child_name, parent_name, version, reviews] = row.as_slice()
                else {
                    return Err(Error::Invalid("team hierarchy request view is invalid"));
                };
                let pending = pending_row(pending)?;
                let requester = pending.requester == input.actor;
                let can_review = !requester && uint(reviews)? != 0;
                let can_approve = can_review
                    && pending.child_version == uint(version)?
                    && requester_can_move(context, &pending)?
                    && hierarchy::valid_parent(
                        context,
                        pending.child,
                        Visibility::Closed,
                        Some(pending.parent),
                    )?;
                Ok(Item {
                    id: pending.id,
                    child: pending.child,
                    child_name: text(child_name)?,
                    parent: pending.parent,
                    parent_name: text(parent_name)?,
                    side: pending.side,
                    can_review,
                    can_approve,
                    can_withdraw: requester,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let next = if rows.rows.len() > limit {
            items.last().map(|item| item.id)
        } else {
            None
        };
        let organization_name = if input.incoming && !items.is_empty() {
            Some(record(context)?.ok_or(Error::NotInitialized)?.name)
        } else {
            None
        };
        Ok(Page {
            items,
            next,
            organization_name,
        })
    }
}
