use super::*;
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
pub(super) fn change(
    context: &mut Transaction<'_>,
    child: u64,
    new_parent: Option<u64>,
    version: u64,
    actor: &RepositoryAuthor,
) -> Result<MutationResult<Outcome>> {
    let fail = |reason: &str| {
        MutationResult::Rejected(Outcome::Rejected {
            reason: reason.into(),
        })
    };
    if validate_number(child).is_err()
        || new_parent.is_some_and(|n| validate_number(n).is_err() || n == child)
    {
        return Ok(fail("An issue cannot be its own parent"));
    }
    if summary(context, child)?.is_none() {
        return Ok(fail("Sub-issue not found"));
    }
    let (old, current) = parent(context, child)?;
    if current != version {
        return Ok(fail("The parent changed; refresh before saving"));
    }
    if let Some(number) = new_parent {
        if summary(context, number)?.is_none() {
            return Ok(fail("Parent issue not found"));
        }
        let mut ancestor = Some(number);
        let mut depth = 0u64;
        while let Some(number) = ancestor {
            if number == child {
                return Ok(fail("Sub-issue relationships cannot form a cycle"));
            }
            depth += 1;
            if depth >= 16 {
                return Ok(fail("Sub-issues support at most 16 levels"));
            }
            ancestor = parent(context, number)?.0;
        }
        // Bound both recursive work and tree depth before reparenting a subtree.
        // Ancestor checks alone would allow a deep subtree to exceed the depth bound.
        let descendants=context.query("WITH RECURSIVE d(child,depth) AS (SELECT child,1 FROM repository_issue_parents WHERE parent=? UNION ALL SELECT p.child,d.depth+1 FROM repository_issue_parents p JOIN d ON p.parent=d.child WHERE d.depth<16 LIMIT 501) SELECT COUNT(*),COALESCE(MAX(depth),0) FROM d",&[integer(child)?])?;
        if result_u64_from_row(&descendants.rows[0], 0)? >= 500
            || result_u64_from_row(&descendants.rows[0], 1)? + depth >= 16
        {
            return Ok(fail("This sub-issue tree exceeds its depth or size limit"));
        }
        let count = context.query(
            "SELECT COUNT(*) FROM repository_issue_parents WHERE parent=? AND child!=?",
            &[integer(number)?, integer(child)?],
        )?;
        if result_u64_from_row(&count.rows[0], 0)? >= 100 {
            return Ok(fail("An issue supports at most 100 direct sub-issues"));
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
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hierarchy {
    pub parent: Option<SubjectSummary>,
    pub version: u64,
    pub progress: Progress,
    pub children: Vec<SubjectSummary>,
    pub next: Option<u64>,
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
        let rows=context.query("SELECT child FROM repository_issue_parents WHERE parent=? AND child>? ORDER BY child LIMIT 51",&[integer(input.issue)?,integer(input.after.unwrap_or(0))?])?;
        let mut children = Vec::new();
        for row in rows.rows.iter().take(50) {
            let number = result_u64_from_row(row, 0)?;
            children.push(summary(context, number)?.ok_or(Error::Invalid("sub-issue is missing"))?);
        }
        let next = if rows.rows.len() > 50 {
            children.last().map(|s| s.subject.number)
        } else {
            None
        };
        Ok(Some(Hierarchy {
            parent: parent.map(|n| summary(context, n)).transpose()?.flatten(),
            version,
            progress: progress(context, input.issue)?,
            children,
            next,
        }))
    }
}
