use super::*;

pub(crate) const MAX_CHILDREN: u8 = 100;
const DEFAULT_PAGE_SIZE: usize = 50;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Progress {
    pub total: u64,
    pub closed: u64,
}
pub(super) fn progress(context: &Transaction<'_>, number: u64) -> Result<Progress> {
    let rows=context.query("SELECT COUNT(*),COALESCE(SUM(i.state),0) FROM repository_issue_parents p JOIN repository_issues i ON i.number=p.child WHERE p.parent=?",&[integer(number)?])?;
    Ok(Progress {
        total: result_u64_from_row(&rows.rows[0], 0)?,
        closed: result_u64_from_row(&rows.rows[0], 1)?,
    })
}
pub(super) fn parent(context: &Transaction<'_>, child: u64) -> Result<(Option<u64>, u64)> {
    let rows = context.query(
        "SELECT parent,version FROM repository_issue_parents WHERE child=?",
        &[integer(child)?],
    )?;
    match rows.rows.first() {
        None => Ok((None, 0)),
        Some(row) => Ok((
            if row[0] == SqlValue::Null {
                None
            } else {
                Some(result_u64_from_row(row, 0)?)
            },
            result_u64_from_row(row, 1)?,
        )),
    }
}
fn summary(context: &Transaction<'_>, number: u64) -> Result<Option<SubjectSummary>> {
    Subject {
        kind: SubjectKind::Issue,
        number,
    }
    .summary(context)
}

fn order_key(parent: u64) -> String {
    format!("projects/v1/sub-issue-order/{parent:016x}")
}

fn children(context: &Transaction<'_>, parent: u64) -> Result<(Vec<SubjectSummary>, bool)> {
    let rows = context.query(
        "SELECT i.number, i.title, i.state FROM repository_issue_parents p
         JOIN repository_issues i ON i.number = p.child WHERE p.parent = ? ORDER BY p.child LIMIT ?",
        &[integer(parent)?, integer(u64::from(MAX_CHILDREN) + 1)?],
    )?;
    if rows.rows.len() > usize::from(MAX_CHILDREN) {
        return Err(Error::Invalid("Sub-issue count exceeds its bound"));
    }
    let mut children = rows
        .rows
        .iter()
        .map(|row| {
            Subject {
                kind: SubjectKind::Issue,
                number: result_u64_from_row(row, 0)?,
            }
            .summary_from_row(&row[1..])
        })
        .collect::<Result<Vec<_>>>()?;
    let order = context.document::<Vec<u64>>(&order_key(parent))?;
    if let Some(order) = order {
        let positions: BTreeMap<_, _> = order
            .into_iter()
            .enumerate()
            .map(|(position, child)| (child, position))
            .collect();
        // Membership comes from the parent edges. Newly attached children
        // follow the saved priorities, and a missing child cannot reappear.
        children.sort_by_key(|child| {
            let number = child.subject.number;
            (
                positions.get(&number).copied().unwrap_or(usize::MAX),
                number,
            )
        });
        return Ok((children, true));
    }
    Ok((children, false))
}

fn order_token(children: &[SubjectSummary]) -> String {
    // Only the exact membership and ordering are preconditions. Editing a
    // child's title or closing it does not invalidate a priority change.
    let mut hash = blake3::Hasher::new();
    for child in children {
        hash.update(&child.subject.number.to_be_bytes());
    }
    hash.finalize().to_hex().to_string()
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Position {
    Before,
    After,
}

pub(super) fn reorder(
    context: &mut Transaction<'_>,
    parent: u64,
    child: u64,
    sibling: u64,
    position: Position,
    expected_order: &str,
) -> Result<MutationResult<Outcome>> {
    validate_number(parent)?;
    let (children, _) = children(context, parent)?;
    if expected_order != order_token(&children) {
        return Ok(rejected(
            "Sub-issues changed. Review their order and try again.",
        ));
    }
    let index = |number| {
        children
            .iter()
            .position(|entry| entry.subject.number == number)
    };
    let (Some(from), Some(anchor)) = (index(child), index(sibling)) else {
        return Ok(rejected(
            "Both issues must be direct sub-issues of this parent",
        ));
    };
    if from == anchor {
        return Ok(rejected("Choose another sub-issue to move relative to"));
    }
    let insertion = anchor + usize::from(matches!(position, Position::After));
    let to = insertion - usize::from(from < insertion);
    if from != to {
        let mut order: Vec<_> = children.iter().map(|entry| entry.subject.number).collect();
        order.remove(from);
        order.insert(to, child);
        context.put_document(&order_key(parent), &order)?;
        advance_revision(context)?;
    }
    Ok(MutationResult::Success(Outcome::Applied {
        project: None,
        version: revision(context)?,
    }))
}

fn rejected(reason: &str) -> MutationResult<Outcome> {
    MutationResult::Rejected(Outcome::Rejected {
        reason: reason.into(),
    })
}

pub(super) fn change(
    context: &mut Transaction<'_>,
    child: u64,
    new_parent: Option<u64>,
    version: u64,
    actor: &RepositoryAuthor,
) -> Result<MutationResult<Outcome>> {
    if validate_number(child).is_err()
        || new_parent.is_some_and(|n| validate_number(n).is_err() || n == child)
    {
        return Ok(rejected("An issue cannot be its own parent"));
    }
    if summary(context, child)?.is_none() {
        return Ok(rejected("Sub-issue not found"));
    }
    let (old, current) = parent(context, child)?;
    if current != version {
        return Ok(rejected("The parent changed; refresh before saving"));
    }
    if let Some(number) = new_parent {
        if summary(context, number)?.is_none() {
            return Ok(rejected("Parent issue not found"));
        }
        let mut ancestor = Some(number);
        let mut depth = 0u64;
        while let Some(number) = ancestor {
            if number == child {
                return Ok(rejected("Sub-issue relationships cannot form a cycle"));
            }
            depth += 1;
            if depth >= 16 {
                return Ok(rejected("Sub-issues support at most 16 levels"));
            }
            ancestor = parent(context, number)?.0;
        }
        // Bound both recursive work and tree depth before reparenting a subtree.
        // Ancestor checks alone would allow a deep subtree to exceed the depth bound.
        let descendants=context.query("WITH RECURSIVE d(child,depth) AS (SELECT child,1 FROM repository_issue_parents WHERE parent=? UNION ALL SELECT p.child,d.depth+1 FROM repository_issue_parents p JOIN d ON p.parent=d.child WHERE d.depth<16 LIMIT 501) SELECT COUNT(*),COALESCE(MAX(depth),0) FROM d",&[integer(child)?])?;
        if result_u64_from_row(&descendants.rows[0], 0)? >= 500
            || result_u64_from_row(&descendants.rows[0], 1)? + depth >= 16
        {
            return Ok(rejected(
                "This sub-issue tree exceeds its depth or size limit",
            ));
        }
        let count = context.query(
            "SELECT COUNT(*) FROM repository_issue_parents WHERE parent=? AND child!=?",
            &[integer(number)?, integer(child)?],
        )?;
        if result_u64_from_row(&count.rows[0], 0)? >= u64::from(MAX_CHILDREN) {
            return Ok(rejected("An issue supports at most 100 direct sub-issues"));
        }
    }
    if old == new_parent {
        return Ok(MutationResult::Success(Outcome::Applied {
            project: None,
            version: current,
        }));
    }
    let version = super::super::collaboration::next_version(current)?;
    context.query("INSERT INTO repository_issue_parents(child,parent,version) VALUES (?,?,?) ON CONFLICT(child) DO UPDATE SET parent=excluded.parent,version=excluded.version",&[integer(child)?,new_parent.map(integer).transpose()?.unwrap_or(SqlValue::Null),integer(version)?])?;
    for (parent, added) in [(old, false), (new_parent, true)] {
        let Some(parent) = parent else { continue };
        let key = order_key(parent);
        if let Some(mut order) = context.document::<Vec<u64>>(&key)? {
            // A child reattached later should enter at the end of the list.
            order.retain(|number| *number != child);
            if added {
                order.push(child);
            }
            if order.is_empty() {
                context.delete_document(&key)?;
            } else {
                context.put_document(&key, &order)?;
            }
        }
    }
    for (number, added) in [(old, false), (new_parent, true)] {
        if let Some(number) = number {
            let parent =
                summary(context, number)?.ok_or(Error::Invalid("parent issue is missing"))?;
            let action = if added {
                activity::Action::ParentAssigned {
                    number,
                    title: parent.title,
                }
            } else {
                activity::Action::ParentRemoved {
                    number,
                    title: parent.title,
                }
            };
            activity::append(context, Target::Issue { number: child }, actor, action)?;
        }
    }
    advance_revision(context)?;
    Ok(MutationResult::Success(Outcome::Applied {
        project: None,
        version,
    }))
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub issue: u64,
    pub after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u8>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hierarchy {
    pub parent: Option<SubjectSummary>,
    pub version: u64,
    pub progress: Progress,
    pub children: Vec<SubjectSummary>,
    pub next: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_token: Option<String>,
}
pub(crate) struct GetHierarchy;
impl Read for GetHierarchy {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 394;
    type Input = Query;
    type Output = Option<Hierarchy>;
    fn execute(context: &mut Transaction<'_>, input: Query) -> Result<Self::Output> {
        preparation::ensure_ready(context)?;
        if summary(context, input.issue)?.is_none() {
            return Ok(None);
        }
        if let Some(after) = input.after {
            validate_number(after)?;
        }
        let (parent, version) = parent(context, input.issue)?;
        let limit = input.limit.map_or(DEFAULT_PAGE_SIZE, usize::from);
        if !(1..=usize::from(MAX_CHILDREN)).contains(&limit) {
            return Err(Error::Invalid(
                "Sub-issue page limit must be between 1 and 100",
            ));
        }
        let (ordered, has_priority) = children(context, input.issue)?;
        let order_token = input.limit.map(|_| order_token(&ordered));
        let start = input.after.map_or(0, |after| {
            if has_priority {
                ordered
                    .iter()
                    .position(|child| child.subject.number == after)
                    .map_or(0, |position| position + 1)
            } else {
                ordered.partition_point(|child| child.subject.number <= after)
            }
        });
        let end = (start + limit).min(ordered.len());
        let next = (end < ordered.len()).then(|| ordered[end - 1].subject.number);
        Ok(Some(Hierarchy {
            parent: parent.map(|n| summary(context, n)).transpose()?.flatten(),
            version,
            progress: progress(context, input.issue)?,
            children: ordered.into_iter().skip(start).take(limit).collect(),
            next,
            order_token,
        }))
    }
}
