use super::*;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryCursor {
    pub organization: Uuid,
    pub revision: u64,
    pub search: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Uuid>,
    pub after: Uuid,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListInput {
    pub actor: Uuid,
    pub search: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Uuid>,
    pub after: Option<QueryCursor>,
    pub limit: u8,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum Page<T, C> {
    Changed,
    Page {
        items: Vec<T>,
        next: Option<C>,
        revision: u64,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "TeamSummary"))]
pub(crate) struct Summary {
    #[serde(flatten)]
    pub view: View,
    pub members: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Listing {
    pub page: Page<Summary, QueryCursor>,
    pub can_create: bool,
}
pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 11;
    type Input = ListInput;
    type Output = Listing;
    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        limit(input.limit)?;
        if input.search.chars().count() > 100 || input.search.chars().any(char::is_control) {
            return Err(Error::Invalid("team search is invalid"));
        }
        let organization = record(context)?.ok_or(Error::Invalid("organization is missing"))?;
        let owner = member(context, input.actor)?
            .is_some_and(|member| member.active && member.role == OrganizationRole::Owner);
        let can_create = owner || policy(context)?.member_creation;
        let search = input.search.to_lowercase();
        if let Some(parent) = input.parent
            && view(context, input.actor, parent)?.is_none()
        {
            return Err(Error::AccessDenied);
        }
        if input.after.as_ref().is_some_and(|cursor| {
            cursor.organization != organization.id
                || cursor.revision != organization.version
                || cursor.search != search
                || cursor.parent != input.parent
        }) {
            return Ok(Listing {
                page: Page::Changed,
                can_create,
            });
        }
        let after = if let Some(cursor) = &input.after {
            let Some(selected) = view(context, input.actor, cursor.after)? else {
                return Ok(Listing {
                    page: Page::Changed,
                    can_create,
                });
            };
            SqlValue::Text(selected.team.name.to_lowercase())
        } else {
            SqlValue::Null
        };
        let rows = context.query(
            &format!("SELECT t.id,t.name,t.description,t.visibility,t.version,t.created_at_ms,t.updated_at_ms,t.parent,
              (SELECT COUNT(*) FROM organization_team_members m JOIN organization_members o ON o.actor=m.actor
                JOIN organization_membership_epochs e ON e.actor=o.actor WHERE m.team=t.id AND {ACTIVE_MEMBERSHIP}),
              CASE WHEN s.membership_epoch=se.epoch THEN s.role END
              FROM organization_teams t LEFT JOIN organization_team_members s ON s.team=t.id AND s.actor=?3
              LEFT JOIN organization_membership_epochs se ON se.actor=s.actor
              WHERE t.deleted_at_ms IS NULL AND instr(t.name_key,?1)>0
                AND (?2=1 OR t.visibility='closed' OR (s.role IS NOT NULL AND s.membership_epoch=se.epoch))
                AND (?4 IS NULL OR t.name_key>?4) AND (?6 IS NULL OR t.parent=?6)
              ORDER BY t.name_key LIMIT ?5"),
            &[SqlValue::Text(search.clone()),SqlValue::Integer(i64::from(owner)),blob(input.actor),after,
              SqlValue::Integer(i64::from(input.limit)+1), input.parent.map_or(SqlValue::Null, blob)],
        )?;
        let mut items = rows
            .rows
            .iter()
            .map(|row| {
                if row.len() != 10 {
                    return Err(Error::Invalid("team summary is invalid"));
                }
                let team = team_row(&row[..8])?;
                let role = if row[9] == SqlValue::Null {
                    None
                } else {
                    Some(Role::parse(&text(&row[9])?)?)
                };
                Ok(Summary {
                    view: View {
                        team,
                        role,
                        can_manage: owner || role == Some(Role::Maintainer),
                        ancestors: None,
                        tree_version: None,
                    },
                    members: uint(&row[8])?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let more = items.len() > usize::from(input.limit);
        items.truncate(usize::from(input.limit));
        let next = if more {
            items.last().map(|item| QueryCursor {
                organization: organization.id,
                revision: organization.version,
                search,
                parent: input.parent,
                after: item.view.team.id,
            })
        } else {
            None
        };
        Ok(Listing {
            page: Page::Page {
                items,
                next,
                revision: organization.version,
            },
            can_create,
        })
    }
}
pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 12;
    type Input = Key;
    type Output = Option<View>;
    fn execute(context: &mut Transaction<'_>, input: Key) -> Result<Self::Output> {
        let Some(mut team) = view(context, input.actor, input.team)? else {
            return Ok(None);
        };
        if team.team.parent.is_some() {
            let mut ancestors = hierarchy::ancestors(context, input.team)?;
            ancestors.retain(|id| *id != input.team);
            team.ancestors = Some(ancestors);
        }
        if hierarchy::has_children(context, input.team)? {
            team.tree_version = Some(record(context)?.ok_or(Error::NotInitialized)?.version);
        }
        Ok(Some(team))
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Collection {
    Members,
    Candidates,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberCursor {
    pub organization: Uuid,
    pub team: Uuid,
    pub revision: u64,
    pub collection: Collection,
    pub after: Uuid,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberInput {
    pub actor: Uuid,
    pub team: Uuid,
    pub after: Option<MemberCursor>,
    pub limit: u8,
}
fn limit(value: u8) -> Result<()> {
    if !(1..=50).contains(&value) {
        return Err(Error::Invalid("team pages require 1–50 entries"));
    }
    Ok(())
}
fn people(
    context: &Transaction<'_>,
    input: MemberInput,
    collection: Collection,
) -> Result<Page<Membership, MemberCursor>> {
    limit(input.limit)?;
    let organization = record(context)?.ok_or(Error::Invalid("organization is missing"))?;
    if let Some(cursor) = &input.after {
        actor(cursor.after)?;
        if cursor.organization != organization.id
            || cursor.team != input.team
            || cursor.revision != organization.version
            || cursor.collection != collection
        {
            return Ok(Page::Changed);
        }
    }
    let after = input
        .after
        .map_or(SqlValue::Null, |cursor| blob(cursor.after));
    let descendants = hierarchy::DESCENDANT_STEP;
    let query=match collection {
        Collection::Members=>format!("WITH RECURSIVE team_tree(root,team) AS (SELECT ?1,?1 UNION {descendants})
            SELECT m.actor, COALESCE(MAX(CASE WHEN m.team=?1 THEN m.role END),'member'),
                COALESCE(MAX(CASE WHEN m.team=?1 THEN m.version END),0)
            FROM team_tree tree JOIN organization_team_members m ON m.team=tree.team
            JOIN organization_members o ON o.actor=m.actor JOIN organization_membership_epochs e ON e.actor=o.actor
            WHERE {ACTIVE_MEMBERSHIP} AND (?2 IS NULL OR m.actor>?2)
            GROUP BY m.actor ORDER BY m.actor LIMIT ?3"),
        Collection::Candidates=>"SELECT o.actor,CASE WHEN m.membership_epoch=e.epoch THEN m.role END,COALESCE(m.version,0) FROM organization_members o JOIN organization_membership_epochs e ON e.actor=o.actor LEFT JOIN organization_team_members m ON m.actor=o.actor AND m.team=?1 WHERE o.active=1 AND (?2 IS NULL OR o.actor>?2) ORDER BY o.actor LIMIT ?3".into(),
    };
    let rows = context.query(
        &query,
        &[
            blob(input.team),
            after,
            SqlValue::Integer(i64::from(input.limit) + 1),
        ],
    )?;
    let mut items = rows
        .rows
        .iter()
        .map(|row| {
            let [person, role, version] = row.as_slice() else {
                return Err(Error::Invalid("team member is invalid"));
            };
            let role = match role {
                SqlValue::Null => None,
                other => Some(Role::parse(&text(other)?)?),
            };
            Ok(Membership {
                actor: uuid(person)?,
                role,
                version: uint(version)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let more = items.len() > usize::from(input.limit);
    items.truncate(usize::from(input.limit));
    let next = if more {
        items.last().map(|item| MemberCursor {
            organization: organization.id,
            team: input.team,
            revision: organization.version,
            collection,
            after: item.actor,
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
pub(crate) struct Members;
impl Read for Members {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 13;
    type Input = MemberInput;
    type Output = Page<Membership, MemberCursor>;
    fn execute(context: &mut Transaction<'_>, input: MemberInput) -> Result<Self::Output> {
        people(context, input, Collection::Members)
    }
}
pub(crate) struct GetMembership;
impl Read for GetMembership {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 14;
    type Input = MemberKey;
    type Output = Membership;
    fn execute(context: &mut Transaction<'_>, input: MemberKey) -> Result<Self::Output> {
        membership(context, input.team, input.member)
    }
}
pub(crate) struct Candidates;
impl Read for Candidates {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 15;
    type Input = MemberInput;
    type Output = Page<Membership, MemberCursor>;
    fn execute(context: &mut Transaction<'_>, input: MemberInput) -> Result<Self::Output> {
        people(context, input, Collection::Candidates)
    }
}
pub(crate) struct GetPolicy;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MentionMembersInput {
    pub actor: Uuid,
    pub team: Uuid,
    pub roster: MentionRoster,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MentionRoster {
    Candidates(Vec<Uuid>),
    Page(Option<MemberCursor>),
}

pub(crate) struct MentionMembers;
impl Read for MentionMembers {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 18;
    type Input = MentionMembersInput;
    type Output = Page<Uuid, MemberCursor>;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        if !view(context, input.actor, input.team)?
            .is_some_and(|visible| visible.team.visibility == Visibility::Closed)
        {
            return Err(Error::AccessDenied);
        }
        let candidates = match input.roster {
            MentionRoster::Candidates(candidates) => candidates,
            MentionRoster::Page(after) => {
                let input = MemberInput {
                    actor: input.actor,
                    team: input.team,
                    after,
                    limit: 50,
                };
                return Ok(match people(context, input, Collection::Members)? {
                    Page::Changed => Page::Changed,
                    Page::Page {
                        items,
                        next,
                        revision,
                    } => Page::Page {
                        items: items.into_iter().map(|member| member.actor).collect(),
                        next,
                        revision,
                    },
                });
            }
        };
        if candidates.len() > 100 {
            return Err(Error::Invalid(
                "Team mention roster is limited to 100 candidates per turn",
            ));
        }
        let revision = record(context)?.ok_or(Error::NotInitialized)?.version;
        let mut recipients = Vec::new();
        let teams = hierarchy::effective_teams_sql();
        let query = format!("WITH RECURSIVE {teams} SELECT 1 FROM effective WHERE team=?2");
        for candidate in candidates {
            actor(candidate)?;
            if !context
                .query(&query, &[blob(candidate), blob(input.team)])?
                .rows
                .is_empty()
            {
                recipients.push(candidate);
            }
        }
        Ok(Page::Page {
            items: recipients,
            next: None,
            revision,
        })
    }
}
impl Read for GetPolicy {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 16;
    type Input = Caller;
    type Output = Policy;
    fn execute(context: &mut Transaction<'_>, _: Caller) -> Result<Policy> {
        ensure_ready(context)?;
        policy(context)
    }
}
