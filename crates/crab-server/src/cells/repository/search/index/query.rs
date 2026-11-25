use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

pub(crate) mod work;

use super::*;
use crate::cells::repository::collaboration::{SubjectState, reactions::Kind};
use crate::cells::repository::search::{
    Expression, Metadata, Query, Term, check_status,
    text::{self, Fields},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Sort {
    #[default]
    Created,
    Updated,
    Comments,
    Reactions,
    ThumbsUp,
    ThumbsDown,
    Laugh,
    Hooray,
    Confused,
    Heart,
    Rocket,
    Eyes,
    Relevance,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
    Asc,
    #[default]
    Desc,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchInput {
    pub before: Option<String>,
    pub limit: u8,
    pub query: Option<String>,
    pub sort: Sort,
    pub direction: Direction,
    pub people: Vec<super::super::people::Binding>,
    pub assignees: Vec<super::super::assignees::Binding>,
    pub review_context: Option<super::super::reviews::status::Context>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_context: Option<check_status::Context>,
    #[serde(default)]
    pub review_teams: super::super::reviews::teams::Bindings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_types: Option<super::super::issue_types::Binding>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub public_reader: bool,
}
impl Default for SearchInput {
    fn default() -> Self {
        Self {
            before: None,
            limit: 30,
            query: None,
            sort: Sort::Created,
            direction: Direction::Desc,
            people: Vec::new(),
            assignees: Vec::new(),
            review_context: None,
            check_context: None,
            review_teams: Default::default(),
            issue_types: None,
            public_reader: false,
        }
    }
}
impl SearchInput {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if !(1..=50).contains(&self.limit) {
            return Err("Discussion page size must be 1–50");
        }
        Query::parse(self.query.as_deref())?;
        if self.issue_types.as_ref().is_some_and(|types| {
            types.organization.is_nil() || types.names.len() > 25 || types.version > MAX_NUMBER
        }) {
            return Err("Issue type search scope is invalid");
        }
        if let Some(cursor) = self.before.as_deref() {
            Cursor::decode(cursor)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateCounts {
    pub open: u64,
    pub closed: u64,
    pub merged: u64,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
    pub counts: StateCounts,
    pub matching: u64,
    pub authored_by_me: bool,
    pub assigned_to_me: bool,
    pub review_requested_from_me: bool,
    pub navigation: super::super::navigation::StateNavigation,
    pub lock_filter: super::super::navigation::LockFilter,
    pub labels: Vec<LabelSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_revision: Option<u64>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LabelSummary {
    pub id: u64,
    pub name: String,
    pub color: String,
}
pub(in crate::cells::repository) trait Labelled {
    fn label_ids(&self) -> &[u64];
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
enum Key {
    Integer(u64),
    Rank(f64),
}
impl Key {
    fn sql(&self) -> crate::cells::Result<SqlValue> {
        match self {
            Self::Integer(value) => integer(*value),
            Self::Rank(value) => Ok(SqlValue::Real(*value)),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    generation: Uuid,
    revision: u64,
    query: String,
    key: Key,
    subject: Subject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at_ms: Option<u64>,
}
impl Cursor {
    fn decode(value: &str) -> Result<Self, &'static str> {
        if value.len() > 1024 {
            return Err("Discussion cursor exceeds its size limit");
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| "Discussion cursor is not valid base64url")?;
        let cursor: Self =
            serde_json::from_slice(&bytes).map_err(|_| "Discussion cursor is invalid")?;
        if cursor.generation.is_nil()
            || cursor.revision > MAX_NUMBER
            || cursor.subject.number == 0
            || cursor.subject.number > MAX_NUMBER
            || cursor.query.len() != 64
            || !cursor
                .query
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            || matches!(cursor.key, Key::Rank(rank) if !rank.is_finite() || rank < 0.0)
            || matches!(cursor.key, Key::Integer(key) if key > MAX_NUMBER)
            || cursor
                .expires_at_ms
                .is_some_and(|expiry| expiry == 0 || expiry > MAX_NUMBER)
        {
            return Err("Discussion cursor fields are invalid");
        }
        Ok(cursor)
    }
    fn encode(&self) -> crate::cells::Result<String> {
        Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?))
    }
}

#[derive(Clone, Copy)]
pub(in crate::cells::repository) struct Metrics {
    pub comments: u64,
    pub reactions: u64,
}
struct Candidate {
    subject: Subject,
    key: Key,
    metrics: Metrics,
}

struct Bindings<'a> {
    values: Vec<SqlValue>,
    people: &'a [super::super::people::Binding],
    assignees: &'a [super::super::assignees::Binding],
    review: Option<super::super::reviews::status::Bound>,
    check_cte: String,
    check_joins: String,
    review_teams: &'a super::super::reviews::teams::Bindings,
    issue_types: &'a Option<super::super::issue_types::Binding>,
    public_reader: bool,
    archived: bool,
    now_ms: i64,
}
impl<'a> Bindings<'a> {
    fn new(
        input: &'a SearchInput,
        prepared: Option<&super::super::reviews::status::Prepared>,
        checks: Option<&CheckCriteria>,
        archived: bool,
        now_ms: i64,
    ) -> crate::cells::Result<Self> {
        let mut values = Self {
            people: &input.people,
            assignees: &input.assignees,
            review: None,
            check_cte: String::new(),
            check_joins: String::new(),
            values: Vec::new(),
            review_teams: &input.review_teams,
            issue_types: &input.issue_types,
            public_reader: input.public_reader,
            archived,
            now_ms,
        };
        if let Some(prepared) = prepared {
            values.review = Some(super::super::reviews::status::Bound::new(
                prepared,
                |value| values.bind(value),
            )?);
        }
        if let Some(checks) = checks {
            let epoch = checks
                .epoch
                .map(|epoch| integer(epoch).map(|value| values.bind(value)))
                .transpose()?;
            let source = epoch.map_or_else(
                || "0".into(),
                |epoch| format!("git_head.epoch_id={epoch} AND git_head.name=pr_source.head_ref"),
            );
            let mut head = "NULLIF(lower(hex(git_head.target_oid)),'')".to_owned();
            let mut forks = "";
            if let Some(input) = input
                .check_context
                .as_ref()
                .filter(|input| !input.fork_heads.is_empty())
            {
                let tips = values.text(&serde_json::to_string(&input.fork_heads)?);
                values.check_cte = format!(
                    "WITH fork_heads AS MATERIALIZED (SELECT CAST(key AS BLOB) AS ref,value AS oid FROM json_each({tips}))"
                );
                forks = "LEFT JOIN fork_heads fork_head ON fork_head.ref=pr_source.head_ref";
                head.push_str(",fork_head.oid");
            }
            values.check_joins = format!(
                "LEFT JOIN discussion_index_pull_sources pr_source ON pr_source.document=d.rowid
                 LEFT JOIN git_projection_refs git_head ON {source}
                 {forks}
                 LEFT JOIN repository_check_rollups ci ON ci.oid=COALESCE(pr_source.merged_head_oid,{head})"
            );
        }
        Ok(values)
    }

    fn bind(&mut self, value: SqlValue) -> String {
        self.values.push(value);
        format!("?{}", self.values.len())
    }
    fn text(&mut self, value: &str) -> String {
        self.bind(SqlValue::Text(value.into()))
    }
    fn person(&mut self, name: &str) -> Option<String> {
        let identity = self
            .people
            .iter()
            .find(|binding| binding.name == name)?
            .identity?;
        Some(self.bind(SqlValue::Blob(identity.to_vec())))
    }

    fn issue_type_matches(&mut self, name: Option<&str>) -> Option<String> {
        let types = self.issue_types.as_ref()?;
        let ids = match name {
            Some(name) => vec![*types.names.get(name)?],
            None => types.names.values().copied().collect(),
        };
        if ids.is_empty() {
            return None;
        }
        let organization = SqlValue::Blob(types.organization.as_bytes().to_vec());
        let epoch = SqlValue::Integer(i64::from(types.ownership_epoch));
        let organization = self.bind(organization);
        let epoch = self.bind(epoch);
        let ids = ids
            .into_iter()
            .map(|id| self.bind(SqlValue::Blob(id.as_bytes().to_vec())))
            .collect::<Vec<_>>()
            .join(",");
        Some(format!(
            "SELECT issue FROM repository_issue_type_index WHERE organization = {organization} AND ownership_epoch = {epoch} AND type_id IN ({ids})"
        ))
    }
}

struct Criteria {
    query: Query,
    archived: Option<bool>,
    review: Option<super::super::reviews::status::Prepared>,
    checks: Option<CheckCriteria>,
    now_ms: u64,
}

#[derive(Serialize)]
struct CheckCriteria {
    epoch: Option<u64>,
    revision: u64,
}

fn criteria(
    context: &mut Transaction<'_>,
    input: &SearchInput,
    subject_kind: Option<SubjectKind>,
) -> crate::cells::Result<Criteria> {
    let query = Query::parse(input.query.as_deref()).map_err(crate::cells::Error::Invalid)?;
    let archived = query
        .needs_lock_state()
        .then(|| crate::cells::repository::settings::archived(context))
        .transpose()?;
    let now_ms = crate::cells::repository::timestamp(context.now_ms())?;
    let review = if query.needs_review_source() {
        Some(super::super::reviews::status::prepare(
            context,
            input.review_context.as_ref(),
        )?)
    } else {
        None
    };
    let checks = if query.uses_check_status() {
        if input.public_reader {
            return Err(crate::cells::Error::Invalid(
                "Check filters require repository access",
            ));
        }
        Some(CheckCriteria {
            epoch: check_status::prepare(context, input.check_context.as_ref())?,
            revision: crate::cells::repository::checks::summary::revision(context)?,
        })
    } else {
        None
    };

    if subject_kind != Some(SubjectKind::Pull) || query.needs_lifecycle() {
        super::super::super::issues::lifecycle::ensure_ready(context)?;
    }
    query
        .validate_people(&input.people)
        .map_err(crate::cells::Error::Invalid)?;
    query
        .validate_assignees(&input.assignees)
        .map_err(crate::cells::Error::Invalid)?;
    input
        .review_teams
        .validate(&query)
        .map_err(crate::cells::Error::Invalid)?;
    Ok(Criteria {
        query,
        archived,
        review,
        checks,
        now_ms,
    })
}

pub(in crate::cells::repository) fn page<T: Serialize + Labelled>(
    context: &mut Transaction<'_>,
    input: SearchInput,
    subject_kind: Option<SubjectKind>,
    mut summary: impl FnMut(&Transaction<'_>, Subject, Metrics) -> crate::cells::Result<T>,
) -> crate::cells::Result<Page<T>> {
    input.validate().map_err(crate::cells::Error::Invalid)?;
    let generation = ensure_ready(context)?;
    let revision = revision(context)?;
    let Criteria {
        query,
        archived,
        review,
        checks,
        now_ms,
    } = criteria(context, &input, subject_kind)?;
    let identity = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT repository_uuid FROM repository_identity WHERE singleton = 1",
            vec![],
        )],
    })?;
    let identity = identity[0]
        .rows
        .first()
        .ok_or(crate::cells::Error::Invalid(
            "discussion repository identity is missing",
        ))?;
    let repository = crate::cells::repository::result_blob(identity, 0)?;
    let digest = blake3::hash(&serde_json::to_vec(&(
        FORMAT,
        "issue-lifecycle/v1",
        repository,
        subject_kind,
        input.sort,
        input.direction,
        &query.expression,
        archived,
        &input.people,
        &input.assignees,
        &input.review_context,
        &input.review_teams,
        &input.issue_types,
        review.as_ref().map(|prepared| &prepared.approvals),
        (input.public_reader, &input.check_context, &checks),
    ))?)
    .to_hex()
    .to_string();
    let cursor = input
        .before
        .as_deref()
        .map(Cursor::decode)
        .transpose()
        .map_err(crate::cells::Error::Invalid)?;
    if cursor.as_ref().is_some_and(|cursor| {
        cursor.generation != generation
            || cursor.revision != revision
            || cursor.query != digest
            || (input.sort == Sort::Relevance) != matches!(cursor.key, Key::Rank(_))
            || cursor.expires_at_ms.is_some_and(|expiry| expiry <= now_ms)
    }) {
        return Err(crate::cells::Error::StaleSearch);
    }
    let mut bindings = Bindings::new(
        &input,
        review.as_ref(),
        checks.as_ref(),
        archived.unwrap_or(false),
        context.now_ms(),
    )?;
    let kind_clause = kind_filter(subject_kind, &mut bindings);
    // Each counter describes its navigation query, even when the current query
    // selects only closed work. Rows and both targets share this native snapshot.
    let (base, navigation) = query.state_navigation();
    let predicate = expression(&base, &mut bindings)?;
    let count_filter = format!("{kind_clause} AND ({predicate})");
    let totals = context.sql(&SqlBatch {
        statements: vec![statement(
            &format!(
                "{} SELECT d.state, COUNT(*) FROM discussion_index d {} WHERE {count_filter} GROUP BY d.state",
                bindings.check_cte, bindings.check_joins
            ),
            bindings.values.clone(),
        )],
    })?;
    let mut counts = StateCounts::default();
    for row in &totals[0].rows {
        let state = crate::cells::repository::result_u64_from_row(row, 0)?;
        let count = crate::cells::repository::result_u64_from_row(row, 1)?;
        match state {
            0 => counts.open = count,
            1 => counts.closed = count,
            2 => counts.merged = count,
            _ => {
                return Err(crate::cells::Error::Invalid(
                    "discussion count state is invalid",
                ));
            }
        }
    }
    counts.closed = counts
        .closed
        .checked_add(counts.merged)
        .filter(|count| *count <= MAX_NUMBER)
        .ok_or(crate::cells::Error::Invalid(
            "discussion count exceeds its range",
        ))?;
    let mut bindings = Bindings::new(
        &input,
        review.as_ref(),
        checks.as_ref(),
        archived.unwrap_or(false),
        context.now_ms(),
    )?;
    let kind_clause = kind_filter(subject_kind, &mut bindings);
    let predicate = expression(&query.expression, &mut bindings)?;
    let filter = format!("{kind_clause} AND ({predicate})");
    let matching = match &navigation {
        super::super::navigation::StateNavigation::State { selected, .. } => match selected {
            super::super::navigation::StateSelection::Open => counts.open,
            super::super::navigation::StateSelection::Closed => counts.closed,
        },
        super::super::navigation::StateNavigation::Results => counts
            .open
            .checked_add(counts.closed)
            .filter(|count| *count <= MAX_NUMBER)
            .ok_or(crate::cells::Error::Invalid(
                "discussion count exceeds its range",
            ))?,
    };
    let order = sort_expression(input.sort, &query.expression, &mut bindings);
    let direction = match input.direction {
        Direction::Asc => "ASC",
        Direction::Desc => "DESC",
    };
    // Issues and PRs allocate numbers independently. The kind breaks equal
    // number/sort ties so crossing a page boundary cannot skip either subject.
    let seek = if let Some(cursor) = cursor {
        let key = bindings.bind(cursor.key.sql()?);
        let number = bindings.bind(integer(cursor.subject.number)?);
        let kind = bindings.bind(SqlValue::Integer(kind(cursor.subject.kind)));
        let operator = if input.direction == Direction::Asc {
            ">"
        } else {
            "<"
        };
        format!(
            "(sort_key {operator} {key} OR (sort_key = {key} AND (number {operator} {number} OR (number = {number} AND kind {operator} {kind}))))"
        )
    } else {
        "1".into()
    };
    let limit = bindings.bind(SqlValue::Integer(i64::from(input.limit) + 1));
    let sql = format!(
        "{} SELECT number, sort_key, comments, reactions, kind FROM (SELECT d.kind, d.number, {order} AS sort_key, d.comments, d.reactions FROM discussion_index d {} WHERE {filter}) WHERE {seek} ORDER BY sort_key {direction}, number {direction}, kind {direction} LIMIT {limit}",
        bindings.check_cte, bindings.check_joins
    );
    let rows = context.sql(&SqlBatch {
        statements: vec![statement(&sql, bindings.values)],
    })?;
    let mut candidates = Vec::with_capacity(rows[0].rows.len());
    for row in &rows[0].rows {
        let key = match row.get(1) {
            Some(SqlValue::Integer(value)) if input.sort != Sort::Relevance => Key::Integer(
                u64::try_from(*value)
                    .map_err(|_| crate::cells::Error::Invalid("discussion sort key is negative"))?,
            ),
            Some(SqlValue::Real(value)) if input.sort == Sort::Relevance && value.is_finite() => {
                Key::Rank(*value)
            }
            _ => {
                return Err(crate::cells::Error::Invalid(
                    "discussion sort key is invalid",
                ));
            }
        };
        candidates.push(Candidate {
            subject: subject(&[row[4].clone(), row[0].clone()])?,
            key,
            metrics: Metrics {
                comments: crate::cells::repository::result_u64_from_row(row, 2)?,
                reactions: crate::cells::repository::result_u64_from_row(row, 3)?,
            },
        });
    }
    let mut items = Vec::with_capacity(usize::from(input.limit));
    let mut bytes = 0usize;
    for candidate in candidates.iter().take(usize::from(input.limit)) {
        let item = summary(context, candidate.subject, candidate.metrics)?;
        let size = serde_json::to_vec(&item)?.len();
        if size > super::super::super::MAX_LIST_OUTPUT_BYTES {
            return Err(crate::cells::Error::Invalid(
                "discussion summary exceeds its byte limit",
            ));
        }
        if bytes + size > super::super::super::MAX_LIST_OUTPUT_BYTES {
            break;
        }
        bytes += size;
        items.push(item);
    }
    let next = if items.len() < candidates.len() {
        let last = candidates
            .get(
                items
                    .len()
                    .checked_sub(1)
                    .ok_or(crate::cells::Error::Invalid(
                        "discussion page cannot make progress",
                    ))?,
            )
            .ok_or(crate::cells::Error::Invalid(
                "discussion page cursor is missing",
            ))?;
        Some(
            Cursor {
                generation,
                revision,
                query: digest,
                key: last.key.clone(),
                subject: last.subject,
                // The earliest expiry can change typed or negated lock predicates
                // without a write. Do not seek through a different result set.
                expires_at_ms: if archived == Some(false) {
                    locks::next_expiry(context)?
                } else {
                    None
                },
            }
            .encode()?,
        )
    } else {
        None
    };
    let selected = items
        .iter()
        .flat_map(|item| item.label_ids().iter().copied())
        .collect::<std::collections::BTreeSet<_>>();
    let mut labels = Vec::with_capacity(selected.len());
    if !selected.is_empty() {
        let placeholders = std::iter::repeat_n("?", selected.len())
            .collect::<Vec<_>>()
            .join(",");
        let values = selected
            .into_iter()
            .map(integer)
            .collect::<crate::cells::Result<Vec<_>>>()?;
        let sql = format!(
            "SELECT number, name, color FROM repository_labels WHERE deleted_version IS NULL AND number IN ({placeholders}) ORDER BY name_key, number"
        );
        let rows = context.sql(&SqlBatch {
            statements: vec![statement(&sql, values)],
        })?;
        for row in &rows[0].rows {
            labels.push(LabelSummary {
                id: crate::cells::repository::result_u64_from_row(row, 0)?,
                name: crate::cells::repository::result_text(row, 1)?,
                color: crate::cells::repository::result_text(row, 2)?,
            });
        }
    }
    Ok(Page {
        items,
        next,
        counts,
        matching,
        authored_by_me: query.authored_by_me(),
        assigned_to_me: query.assigned_to_me(),
        review_requested_from_me: query.review_requested_from_me(),
        navigation,
        lock_filter: query.lock_filter(),
        labels,
        check_revision: checks.map(|checks| checks.revision),
    })
}

fn kind_filter(subject_kind: Option<SubjectKind>, bindings: &mut Bindings<'_>) -> String {
    match subject_kind {
        Some(subject_kind) => {
            let value = bindings.bind(SqlValue::Integer(kind(subject_kind)));
            format!("d.kind = {value}")
        }
        None => "1".into(),
    }
}

fn expression(
    expression: &Expression,
    bindings: &mut Bindings<'_>,
) -> crate::cells::Result<String> {
    scoped_expression(expression, bindings, Fields::ALL)
}
fn scoped_expression(
    expression: &Expression,
    bindings: &mut Bindings<'_>,
    fields: Fields,
) -> crate::cells::Result<String> {
    Ok(match expression {
        Expression::All => "1".into(),
        Expression::Not(inner) => format!("NOT ({})",scoped_expression(inner, bindings, fields)?),
        Expression::Scope { fields: selected, expression } => scoped_expression(expression, bindings, *selected)?,
        Expression::And(parts) | Expression::Or(parts) => {
            let operator = if matches!(expression, Expression::And(_)) { " AND " } else { " OR " };
            parts.iter().map(|part| scoped_expression(part, bindings, fields).map(|value|format!("({value})"))).collect::<crate::cells::Result<Vec<_>>>()?.join(operator)
        }
        Expression::Term { value: term, .. } => match term {
            Term::Count(filter) => filter.sql(|value| bindings.bind(SqlValue::Integer(value))),
            Term::Text(value) => text::predicate(value, fields, |value| bindings.text(value)),
            Term::Label(names) => {
                let values = names.iter().map(|name|bindings.text(name)).collect::<Vec<_>>().join(",");
                format!("EXISTS (SELECT 1 FROM discussion_index_labels dl JOIN repository_labels l ON l.number = dl.label WHERE dl.document = d.rowid AND l.deleted_version IS NULL AND l.name_key IN ({values}))")
            }
            Term::Author(name) => bindings.person(name).map_or_else(|| "0".into(), |value| {
                format!("d.rowid IN (SELECT a.document FROM discussion_index_authors a WHERE a.identity = {value})")
            }),
            Term::Commenter(name) => bindings.person(name).map_or_else(|| "0".into(), |value| {
                format!("EXISTS (SELECT 1 FROM discussion_index_commenters c WHERE c.identity = {value} AND c.kind = d.kind AND c.discussion = d.number)")
            }),
            Term::Assignee(name) => {
                if name == "*" {
                    return Ok("EXISTS (SELECT 1 FROM discussion_index_assignees a WHERE a.document = d.rowid)".into());
                }
                let id = name.strip_prefix("actor/").map(str::to_owned).or_else(|| bindings.assignees.iter().find(|binding| binding.name == *name).and_then(|binding| binding.id).map(|id| id.to_string()));
                match id {
                    Some(id) => {
                        let value = bindings.text(&id);
                        format!("d.rowid IN (SELECT a.document FROM discussion_index_assignees a WHERE a.actor = {value})")
                    }
                    None => "0".into(),
                }
            }
            Term::ReviewStatus(super::super::reviews::status::Status::None) => super::super::reviews::status::UNREVIEWED.into(),
            Term::ReviewStatus(super::super::reviews::status::Status::ChangesRequested) => super::super::reviews::status::CHANGES_REQUESTED.into(),
            Term::ReviewStatus(status)=> bindings.review.as_ref().ok_or(crate::cells::Error::Invalid("Review source was not bound"))?.predicate(*status),
            Term::CheckStatus(status) => format!(
                "d.kind=1 AND (pr_source.merged_head_oid IS NOT NULL OR git_head.target_oid IS NOT NULL {}) AND ({})",
                if bindings.check_cte.is_empty() { "" } else { "OR fork_head.oid IS NOT NULL" },
                status.predicate(),
            ),
            Term::BaseBranch(branch) => {
                let reference = bindings.text(&format!("refs/heads/{branch}"));
                format!("d.kind = 1 AND EXISTS (SELECT 1 FROM repository_pulls p WHERE p.number = d.number AND p.base_ref = {reference})")
            }
            Term::HeadBranch(prefix) if bindings.public_reader => {
                // Public rows omit fork heads. The projected name begins with
                // the fork UUID for those rows, so only local heads can match.
                let reference = bindings.bind(SqlValue::Blob(format!("refs/heads/{prefix}").into_bytes()));
                format!("d.kind = 1 AND EXISTS (SELECT 1 FROM discussion_index_pull_sources p WHERE p.document = d.rowid AND instr(p.head_ref, {reference}) = 1)")
            }
            Term::HeadBranch(prefix) => {
                let reference = bindings.text(&format!("refs/heads/{prefix}"));
                format!("d.kind = 1 AND EXISTS (SELECT 1 FROM repository_pulls p WHERE p.number = d.number AND instr(p.head_ref, {reference}) = 1)")
            }
            Term::Reviewer { kind, name } => {
                use super::super::reviews::Kind;
                let direct = bindings.person(name).map_or_else(|| "0".into(), |value| {
                    let predicate = kind.predicate();
                    format!("d.kind = 1 AND d.number IN (SELECT r.pull FROM discussion_index_reviewers r WHERE r.identity = {value} AND ({predicate}))")
                });
                if *kind == Kind::ReviewRequested && name == "@me" && !bindings.review_teams.mine.is_empty() {
                    let teams = bindings.text(&serde_json::to_string(&bindings.review_teams.mine)?);
                    let team = format!("EXISTS (SELECT 1 FROM discussion_index_team_requests t WHERE t.document=d.rowid AND t.team IN (SELECT value FROM json_each({teams})))");
                    format!("(({direct}) OR (d.kind=1 AND {team}))")
                } else {
                    direct
                }
            }
            Term::TeamReviewRequested(name) => {
                if let Some(team) = bindings.review_teams.named(name) {
                    let value = bindings.text(&team.to_string());
                    format!("d.kind=1 AND EXISTS (SELECT 1 FROM discussion_index_team_requests t WHERE t.document=d.rowid AND t.team={value})")
                } else {
                    "0".into()
                }
            }
            Term::Milestone(title) => {
                let value = bindings.text(title);
                format!("EXISTS (SELECT 1 FROM discussion_index_milestones dm JOIN discussion_index_milestone_catalog m ON m.number = dm.milestone WHERE dm.document = d.rowid AND m.title_key = {value})")
            }
            Term::Reason(reason)=>format!("d.kind=0 AND EXISTS (SELECT 1 FROM repository_issue_lifecycle il WHERE il.issue=d.number AND il.state_reason={})",bindings.text(reason.name())),
            Term::Missing(Metadata::Reason)=>"d.kind=0 AND EXISTS (SELECT 1 FROM repository_issue_lifecycle il WHERE il.issue=d.number AND il.state_reason IS NULL)".into(),
            Term::State(SubjectState::Open) => "d.state = 0".into(),
            Term::State(SubjectState::Closed) => "d.state <> 0".into(),
            Term::State(SubjectState::Merged) => "d.state = 2".into(),
            Term::Kind(subject_kind) => format!("d.kind = {}",kind(*subject_kind)),
            Term::Unmerged => "d.kind = 1 AND d.state <> 2".into(),
            Term::Draft(draft) => format!("d.kind = 1 AND d.draft = {}",u8::from(*draft)),
            Term::Locked(locked) => {
                if bindings.archived {
                    u8::from(*locked).to_string()
                } else {
                    let now = bindings.bind(SqlValue::Integer(bindings.now_ms));
                    let negate = if *locked { "" } else { "NOT " };
                    format!("{negate}EXISTS (SELECT 1 FROM discussion_index_locks l WHERE l.document=d.rowid AND (l.expires_at_ms IS NULL OR l.expires_at_ms > {now}))")
                }
            }
            Term::Missing(Metadata::Label) => "NOT EXISTS (SELECT 1 FROM discussion_index_labels dl JOIN repository_labels l ON l.number = dl.label WHERE dl.document = d.rowid AND l.deleted_version IS NULL)".into(),
            Term::Missing(Metadata::Assignee) => "NOT EXISTS (SELECT 1 FROM discussion_index_assignees a WHERE a.document = d.rowid)".into(),
            Term::Missing(Metadata::Milestone) => "NOT EXISTS (SELECT 1 FROM discussion_index_milestones m WHERE m.document = d.rowid)".into(),
            Term::IssueType(name) => bindings.issue_type_matches(Some(name)).map_or_else(
                || "0".into(),
                |matches| format!("d.kind = 0 AND d.number IN ({matches})"),
            ),
            Term::Missing(Metadata::IssueType) => bindings.issue_type_matches(None).map_or_else(
                || "d.kind = 0".into(),
                |matches| format!("d.kind = 0 AND d.number NOT IN ({matches})"),
            ),
        }
    })
}

fn sort_expression(sort: Sort, expression: &Expression, bindings: &mut Bindings<'_>) -> String {
    match sort {
        Sort::Created => "d.created_at_ms".into(),
        Sort::Updated => "d.updated_at_ms".into(),
        Sort::Comments => "d.comments".into(),
        Sort::Reactions => "d.reactions".into(),
        Sort::Relevance => {
            let mut terms = Vec::new();
            positive_text(expression, false, Fields::ALL, &mut terms);
            let ranks = terms
                .into_iter()
                .map(|(value, fields)| text::relevance(value, fields, |value| bindings.text(value)))
                .collect::<Vec<_>>();
            if ranks.is_empty() {
                "0.0".into()
            } else {
                format!("({})", ranks.join(" + "))
            }
        }
        Sort::ThumbsUp => reaction_expression(Kind::ThumbsUp, bindings),
        Sort::ThumbsDown => reaction_expression(Kind::ThumbsDown, bindings),
        Sort::Laugh => reaction_expression(Kind::Laugh, bindings),
        Sort::Hooray => reaction_expression(Kind::Hooray, bindings),
        Sort::Confused => reaction_expression(Kind::Confused, bindings),
        Sort::Heart => reaction_expression(Kind::Heart, bindings),
        Sort::Rocket => reaction_expression(Kind::Rocket, bindings),
        Sort::Eyes => reaction_expression(Kind::Eyes, bindings),
    }
}
fn reaction_expression(kind: Kind, bindings: &mut Bindings<'_>) -> String {
    let kind = bindings.text(kind.key());
    format!(
        "COALESCE((SELECT count FROM discussion_index_reaction_counts r WHERE r.document = d.rowid AND r.kind = {kind}),0)"
    )
}
fn positive_text<'a>(
    expression: &'a Expression,
    negative: bool,
    fields: Fields,
    output: &mut Vec<(&'a str, Fields)>,
) {
    match expression {
        Expression::Term {
            value: Term::Text(text),
            ..
        } if !negative => output.push((text, fields)),
        Expression::Scope {
            fields: selected,
            expression,
        } => positive_text(expression, negative, *selected, output),
        Expression::Not(inner) => positive_text(inner, !negative, fields, output),
        Expression::And(parts) | Expression::Or(parts) => {
            for part in parts {
                positive_text(part, negative, fields, output);
            }
        }
        _ => (),
    }
}
