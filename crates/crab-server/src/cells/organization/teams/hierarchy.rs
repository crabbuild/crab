use super::*;

pub(super) fn effective_teams_sql() -> String {
    format!(
        "direct(team) AS (
    SELECT m.team FROM organization_team_members m
    JOIN organization_teams t ON t.id=m.team
    JOIN organization_members o ON o.actor=m.actor
    JOIN organization_membership_epochs e ON e.actor=o.actor
    WHERE m.actor=?1 AND t.deleted_at_ms IS NULL AND {ACTIVE_MEMBERSHIP}
), effective(team) AS (
    SELECT team FROM direct
    UNION
    SELECT parent.id FROM effective
    JOIN organization_teams child ON child.id=effective.team
    JOIN organization_teams parent ON parent.id=child.parent
    WHERE parent.deleted_at_ms IS NULL
)"
    )
}

pub(in crate::cells::organization) const DESCENDANT_STEP: &str =
    "SELECT team_tree.root, child.id FROM team_tree
    JOIN organization_teams child ON child.parent=team_tree.team
    WHERE child.deleted_at_ms IS NULL";

pub(super) fn ancestors(context: &Transaction<'_>, id: Uuid) -> Result<Vec<Uuid>> {
    let rows = context.query(
        "WITH RECURSIVE lineage(id) AS (
            SELECT id FROM organization_teams WHERE id=?1 AND deleted_at_ms IS NULL
            UNION
            SELECT parent.id FROM lineage
            JOIN organization_teams child ON child.id=lineage.id
            JOIN organization_teams parent ON parent.id=child.parent
            WHERE parent.deleted_at_ms IS NULL
         ) SELECT id FROM lineage",
        &[blob(id)],
    )?;
    rows.rows
        .iter()
        .map(|row| match row.as_slice() {
            [id] => uuid(id),
            _ => Err(Error::Invalid("team ancestor is invalid")),
        })
        .collect()
}

pub(super) fn has_children(context: &Transaction<'_>, team: Uuid) -> Result<bool> {
    Ok(!context
        .query(
            "SELECT 1 FROM organization_teams WHERE parent=? AND deleted_at_ms IS NULL LIMIT 1",
            &[blob(team)],
        )?
        .rows
        .is_empty())
}

pub(super) fn can_choose_parent(
    context: &Transaction<'_>,
    actor: Uuid,
    owner: bool,
    parent: Option<Uuid>,
) -> Result<bool> {
    let Some(parent) = parent else {
        return Ok(true);
    };
    Ok(owner || view(context, actor, parent)?.is_some_and(|team| team.can_manage))
}

pub(super) fn valid_parent(
    context: &Transaction<'_>,
    child: Uuid,
    visibility: Visibility,
    parent: Option<Uuid>,
) -> Result<bool> {
    let Some(parent) = parent else {
        return Ok(true);
    };
    if child == parent || parent.is_nil() || visibility == Visibility::Secret {
        return Ok(false);
    }
    if team(context, parent)?.is_none_or(|team| team.visibility == Visibility::Secret) {
        return Ok(false);
    }
    Ok(!ancestors(context, parent)?.contains(&child))
}
