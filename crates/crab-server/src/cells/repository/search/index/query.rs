use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

use super::*;
use crate::cells::repository::collaboration::{SubjectState, reactions::Kind};
use crate::cells::repository::search::{
    Expression, Metadata, Query, Term,
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
        }
    }
}
impl SearchInput {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if !(1..=50).contains(&self.limit) {
            return Err("Discussion page size must be 1–50");
        }
        Query::parse(self.query.as_deref())?;
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
    pub labels: Vec<LabelSummary>,
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

#[derive(Default)]
struct Bindings<'a> {
    values: Vec<SqlValue>,
    people: &'a [super::super::people::Binding],
    assignees: &'a [super::super::assignees::Binding],
    review: Option<super::super::reviews::status::Bound>,
}
impl<'a> Bindings<'a> {
    fn new(
        input: &'a SearchInput,
        prepared: Option<&super::super::reviews::status::Prepared>,
    ) -> crate::cells::Result<Self> {
        let mut values = Self {
            people: &input.people,
            assignees: &input.assignees,
            ..Self::default()
        };
        if let Some(prepared) = prepared {
            values.review = Some(super::super::reviews::status::Bound::new(
                prepared,
                |value| values.bind(value),
            )?);
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
    let query = Query::parse(input.query.as_deref()).map_err(crate::cells::Error::Invalid)?;
    let review = if query.needs_review_source() {
        Some(super::super::reviews::status::prepare(
            context,
            input.review_context.as_ref(),
        )?)
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
        &input.people,
        &input.assignees,
        &input.review_context,
        review.as_ref().map(|prepared| &prepared.rules),
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
    }) {
        return Err(crate::cells::Error::StaleSearch);
    }
    let mut bindings = Bindings::new(&input, review.as_ref())?;
    let kind_clause = kind_filter(subject_kind, &mut bindings);
    // Each counter describes its navigation query, even when the current query
    // selects only closed work. Rows and both targets share this native snapshot.
    let (base, navigation) = query.state_navigation();
    let predicate = expression(&base, &mut bindings)?;
    let count_filter = format!("{kind_clause} AND ({predicate})");
    let totals = context.sql(&SqlBatch {
        statements: vec![statement(
            &format!(
                "SELECT d.state, COUNT(*) FROM discussion_index d WHERE {count_filter} GROUP BY d.state"
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
    let mut bindings = Bindings::new(&input, review.as_ref())?;
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
        "SELECT number, sort_key, comments, reactions, kind FROM (SELECT d.kind, d.number, {order} AS sort_key, d.comments, d.reactions FROM discussion_index d WHERE {filter}) WHERE {seek} ORDER BY sort_key {direction}, number {direction}, kind {direction} LIMIT {limit}"
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
        labels,
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
                format!("EXISTS (SELECT 1 FROM discussion_index_authors a WHERE a.document = d.rowid AND a.identity = {value})")
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
                        format!("EXISTS (SELECT 1 FROM discussion_index_assignees a WHERE a.document = d.rowid AND a.actor = {value})")
                    }
                    None => "0".into(),
                }
            }
            Term::ReviewStatus(super::super::reviews::status::Status::None) => super::super::reviews::status::UNREVIEWED.into(),
            Term::ReviewStatus(super::super::reviews::status::Status::ChangesRequested) => super::super::reviews::status::CHANGES_REQUESTED.into(),
            Term::ReviewStatus(status)=> bindings.review.as_ref().ok_or(crate::cells::Error::Invalid("Review source was not bound"))?.predicate(*status),
            Term::Reviewer { kind, name } => bindings.person(name).map_or_else(|| "0".into(), |value| {
                let predicate = kind.predicate();
                format!("d.kind = 1 AND EXISTS (SELECT 1 FROM discussion_index_reviewers r WHERE r.pull = d.number AND r.identity = {value} AND ({predicate}))")
            }),
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
            Term::Missing(Metadata::Label) => "NOT EXISTS (SELECT 1 FROM discussion_index_labels dl JOIN repository_labels l ON l.number = dl.label WHERE dl.document = d.rowid AND l.deleted_version IS NULL)".into(),
            Term::Missing(Metadata::Assignee) => "NOT EXISTS (SELECT 1 FROM discussion_index_assignees a WHERE a.document = d.rowid)".into(),
            Term::Missing(Metadata::Milestone) => "NOT EXISTS (SELECT 1 FROM discussion_index_milestones m WHERE m.document = d.rowid)".into(),
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
