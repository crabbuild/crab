use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Caller {
    pub actor: Uuid,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberKey {
    pub actor: Uuid,
    pub member: Uuid,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Collection {
    Members,
    Invitations,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageCursor {
    pub collection: Collection,
    pub organization: Uuid,
    pub revision: u64,
    pub after: Uuid,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageQuery {
    pub actor: Uuid,
    pub after: Option<PageCursor>,
    pub limit: u8,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum Page<T> {
    Changed,
    Page {
        items: Vec<T>,
        next: Option<PageCursor>,
        revision: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct View {
    pub organization: Organization,
    pub role: Role,
}
pub(crate) struct GetOrganization;
impl Read for GetOrganization {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 1;
    type Input = Caller;
    type Output = Option<View>;
    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        let role = member(context, input.actor)?
            .filter(|member| member.active)
            .ok_or(Error::AccessDenied)?
            .role;
        Ok(record(context)?.map(|organization| View { organization, role }))
    }
}
pub(crate) struct GetMember;
impl Read for GetMember {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 3;
    type Input = MemberKey;
    type Output = Option<Member>;
    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        member(context, input.member)
    }
}
pub(crate) struct ListMembers;
impl Read for ListMembers {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 2;
    type Input = PageQuery;
    type Output = Page<Member>;
    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        if !(1..=50).contains(&input.limit) {
            return Err(Error::Invalid(
                "organization member pages require 1–50 entries",
            ));
        }
        let organization =
            record(context)?.ok_or(Error::Invalid("organization record is missing"))?;
        if let Some(cursor) = &input.after {
            actor(cursor.after)?;
            if cursor.collection != Collection::Members
                || cursor.organization != organization.id
                || cursor.revision != organization.version
            {
                return Ok(Page::Changed);
            }
        }
        let mut values = Vec::new();
        let predicate = if let Some(cursor) = &input.after {
            values.push(blob(cursor.after));
            " AND actor > ?"
        } else {
            ""
        };
        values.push(SqlValue::Integer(i64::from(input.limit) + 1));
        let sql = format!(
            "SELECT actor, role, active, version FROM organization_members WHERE active = 1{predicate} ORDER BY actor LIMIT ?"
        );
        let mut items = context
            .query(&sql, &values)?
            .rows
            .iter()
            .map(|row| member_row(row))
            .collect::<Result<Vec<_>>>()?;
        let more = items.len() > usize::from(input.limit);
        items.truncate(usize::from(input.limit));
        let next = if more {
            items.last().map(|member| PageCursor {
                collection: Collection::Members,
                organization: organization.id,
                revision: organization.version,
                after: member.actor,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActivityQuery {
    pub actor: Uuid,
    pub before: Option<u64>,
    pub limit: u8,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActivityPage {
    pub items: Vec<Event>,
    pub next: Option<u64>,
}
pub(crate) struct ListActivity;
impl Read for ListActivity {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 4;
    type Input = ActivityQuery;
    type Output = ActivityPage;
    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        if !(1..=50).contains(&input.limit)
            || input
                .before
                .is_some_and(|value| value == 0 || value > MAX_VERSION)
        {
            return Err(Error::Invalid("organization activity page is invalid"));
        }
        let before = input.before.map(|value| format!("{EVENTS}{value:016x}"));
        let mut items =
            context.documents::<Event>(EVENTS, before.as_deref(), usize::from(input.limit) + 1)?;
        for (key, event) in &items {
            if *key != format!("{EVENTS}{:016x}", event.revision) {
                return Err(Error::Invalid("organization activity index is invalid"));
            }
        }
        let more = items.len() > usize::from(input.limit);
        items.truncate(usize::from(input.limit));
        let next = if more {
            items.last().map(|(_, event)| event.revision)
        } else {
            None
        };
        Ok(ActivityPage {
            items: items.into_iter().map(|(_, event)| event).collect(),
            next,
        })
    }
}
