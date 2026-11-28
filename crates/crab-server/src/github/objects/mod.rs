//! GitHub GraphQL objects resolved from native API data.
//!
//! Objects hold the native JSON a read already returned and load further
//! data only when a client selects a field that needs it. Every load is a
//! native request through the shared boundary, so authorization is rechecked
//! for each repository an operation touches.

mod actors;
mod discussions;
mod mutations;
mod repository;

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::{Value, json};

pub(crate) use mutations::Mutation;

use super::{
    Context,
    graphql::{FieldError, FieldRequest, Object, Resolved},
    ids::NodeId,
    native::Failure,
};

pub(super) use discussions::{Issue, PullRequest};
pub(super) use repository::Repository;

pub(super) type R = Resolved<Context>;
pub(super) type Field<'a> = FieldRequest<'a>;
pub(super) type Result<T> = std::result::Result<T, FieldError>;

/// GitHub's largest connection page.
pub(super) const MAX_PAGE: usize = 100;
const DEFAULT_PAGE: usize = 30;

impl From<Failure> for FieldError {
    fn from(failure: Failure) -> Self {
        match failure.status {
            StatusCode::NOT_FOUND => Self::not_found(failure.message),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Self::forbidden(failure.message),
            StatusCode::BAD_REQUEST
            | StatusCode::CONFLICT
            | StatusCode::PRECONDITION_FAILED
            | StatusCode::UNPROCESSABLE_ENTITY => Self::unprocessable(failure.message),
            _ => Self::new(failure.message),
        }
    }
}

pub(crate) struct Query;

#[async_trait]
impl Object<Context> for Query {
    fn typename(&self) -> &str {
        "Query"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        match field.name {
            "viewer" => actors::viewer(context).await,
            "repository" => {
                let (owner, name) = (required(field, "owner")?, required(field, "name")?);
                Ok(R::object(Repository::find(context, owner, name).await?))
            }
            "repositoryOwner" => actors::owner(context, required(field, "login")?).await,
            "user" => actors::user(context, required(field, "login")?).await,
            "organization" => actors::organization(context, required(field, "login")?).await,
            "node" => node(context, required(field, "id")?).await,
            "nodes" => {
                let ids = field
                    .argument("ids")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if ids.len() > MAX_PAGE {
                    return Err(FieldError::typed(
                        "EXCESSIVE_PAGE_SIZE",
                        "You may not request more than 100 nodes",
                    ));
                }
                let mut nodes = Vec::with_capacity(ids.len());
                for id in ids {
                    let resolved = match id.as_str() {
                        Some(id) => node(context, id).await.unwrap_or_else(|_| R::null()),
                        None => R::null(),
                    };
                    nodes.push(resolved);
                }
                Ok(R::List(nodes))
            }
            "search" => discussions::search(context, field).await,
            "rateLimit" => Ok(R::object(JsonObject::new(
                "RateLimit",
                json!({
                    "cost": 1, "limit": 5000, "nodeCount": 0, "remaining": 5000, "used": 0,
                    "resetAt": timestamp_seconds(super::now_seconds() + 3600),
                }),
            ))),
            _ => Ok(R::null()),
        }
    }
}

/// Resolves a node ID. Each kind loads through its repository, so a node
/// from a repository the caller cannot read resolves to nothing.
pub(super) async fn node(context: &Context, id: &str) -> Result<R> {
    let missing = || {
        FieldError::not_found(format!(
            "Could not resolve to a node with the global id of '{id}'"
        ))
    };
    let node = NodeId::decode(id).ok_or_else(missing)?;
    match &node {
        NodeId::User(actor) => return actors::user_by_actor(context, *actor).await,
        NodeId::Organization(id) => return actors::organization_by_id(context, *id).await,
        NodeId::Team(organization, team) => {
            return actors::team_by_id(context, *organization, *team).await;
        }
        _ => {}
    }
    let repository = match node.repository() {
        Some(id) => Repository::by_id(context, id).await?.ok_or_else(missing)?,
        None => return Err(missing()),
    };
    Ok(match node {
        NodeId::Repository(_) => R::object(repository),
        NodeId::Issue(_, number) => R::object(
            Issue::find(context, &repository, number)
                .await?
                .ok_or_else(missing)?,
        ),
        NodeId::Pull(_, number) => R::object(
            PullRequest::find(context, &repository, number)
                .await?
                .ok_or_else(missing)?,
        ),
        NodeId::Comment {
            discussion,
            number,
            comment,
            ..
        } => discussions::comment(context, &repository, discussion, number, comment)
            .await?
            .ok_or_else(missing)?,
        NodeId::Label(_, number) => repository::label_by_number(context, &repository, number)
            .await?
            .ok_or_else(missing)?,
        _ => return Err(missing()),
    })
}

pub(super) fn required<'a>(field: &'a Field<'_>, name: &str) -> Result<&'a str> {
    field
        .string(name)
        .ok_or_else(|| FieldError::unprocessable(format!("Argument '{name}' is required")))
}

/// A shared object, so a repository loaded once can answer for every issue
/// and pull request beneath it.
pub(super) struct Shared(Arc<dyn Object<Context>>);

#[async_trait]
impl Object<Context> for Shared {
    fn typename(&self) -> &str {
        self.0.typename()
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        self.0.resolve(context, field).await
    }
}

/// A leaf-only object whose fields are precomputed JSON values.
pub(super) struct JsonObject {
    typename: &'static str,
    value: Value,
}

impl JsonObject {
    pub(super) fn new(typename: &'static str, value: Value) -> Self {
        Self { typename, value }
    }
}

#[async_trait]
impl Object<Context> for JsonObject {
    fn typename(&self) -> &str {
        self.typename
    }

    async fn resolve(&self, _: &Context, field: &Field<'_>) -> Result<R> {
        Ok(R::Leaf(
            self.value.get(field.name).cloned().unwrap_or(Value::Null),
        ))
    }
}

/// A requested connection page.
pub(super) struct Page {
    pub(super) size: usize,
    pub(super) after: Option<String>,
    /// `last` without `before` selects the newest entries.
    pub(super) last: bool,
}

pub(super) fn page(field: &Field<'_>) -> Result<Page> {
    let first = field.int("first");
    let last = field.int("last");
    if field.argument("before").is_some() {
        return Err(FieldError::unprocessable(
            "Backward pagination with `before` is not supported",
        ));
    }
    let size = match (first, last) {
        (Some(_), Some(_)) => {
            return Err(FieldError::unprocessable(
                "Passing both `first` and `last` is not supported",
            ));
        }
        (Some(value), None) | (None, Some(value)) => value,
        (None, None) => DEFAULT_PAGE as i64,
    };
    let size = usize::try_from(size)
        .map_err(|_| FieldError::unprocessable("`first` and `last` must be non-negative"))?;
    if size > MAX_PAGE {
        return Err(FieldError::typed(
            "EXCESSIVE_PAGE_SIZE",
            format!(
                "Requesting {size} records on the `{}` connection exceeds the `first` limit of 100 records.",
                field.name
            ),
        ));
    }
    Ok(Page {
        size,
        after: field.string("after").map(str::to_owned),
        last: last.is_some(),
    })
}

/// A connection page. Edge cursors resume after their own node.
pub(super) struct Connection {
    typename: &'static str,
    edge: &'static str,
    nodes: Vec<(Arc<dyn Object<Context>>, String)>,
    total: usize,
    has_next: bool,
    // Count fields of specialized connections such as search results.
    scalars: serde_json::Map<String, Value>,
    lists: Vec<(&'static str, &'static str, Vec<Value>)>,
}

impl Connection {
    pub(super) fn new(
        typename: &'static str,
        edge: &'static str,
        nodes: Vec<(Arc<dyn Object<Context>>, String)>,
        total: usize,
        has_next: bool,
    ) -> Self {
        Self {
            typename,
            edge,
            nodes,
            total,
            has_next,
            scalars: serde_json::Map::new(),
            lists: Vec::new(),
        }
    }

    /// Slices an already complete, ordered list into the requested page.
    pub(super) fn slice(
        typename: &'static str,
        edge: &'static str,
        items: Vec<Arc<dyn Object<Context>>>,
        page: &Page,
    ) -> Result<Self> {
        let total = items.len();
        let start = match &page.after {
            Some(cursor) => offset(cursor)? + 1,
            None if page.last => total.saturating_sub(page.size),
            None => 0,
        };
        let end = (start + page.size).min(total);
        let nodes = items
            .into_iter()
            .enumerate()
            .skip(start)
            .take(end.saturating_sub(start))
            .map(|(index, item)| (item, offset_cursor(index)))
            .collect();
        Ok(Self::new(typename, edge, nodes, total, end < total))
    }

    pub(super) fn scalar(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.scalars.insert(name.to_owned(), value.into());
        self
    }

    /// A list field of leaf-only objects, such as per-state check counts.
    pub(super) fn objects(
        mut self,
        field: &'static str,
        typename: &'static str,
        items: Vec<Value>,
    ) -> Self {
        self.lists.push((field, typename, items));
        self
    }
}

pub(super) fn offset_cursor(index: usize) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(format!("cursor:{index}"))
}

fn offset(cursor: &str) -> Result<usize> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(cursor)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| text.strip_prefix("cursor:")?.parse().ok())
        .ok_or_else(|| {
            FieldError::unprocessable(format!("`{cursor}` does not appear to be a valid cursor."))
        })
}

#[async_trait]
impl Object<Context> for Connection {
    fn typename(&self) -> &str {
        self.typename
    }

    async fn resolve(&self, _: &Context, field: &Field<'_>) -> Result<R> {
        Ok(match field.name {
            "nodes" => R::List(
                self.nodes
                    .iter()
                    .map(|(node, _)| R::Object(Box::new(Shared(Arc::clone(node)))))
                    .collect(),
            ),
            "edges" => R::List(
                self.nodes
                    .iter()
                    .map(|(node, cursor)| {
                        R::object(Edge {
                            typename: self.edge,
                            node: Arc::clone(node),
                            cursor: cursor.clone(),
                        })
                    })
                    .collect(),
            ),
            "pageInfo" => R::object(JsonObject::new(
                "PageInfo",
                json!({
                    "hasNextPage": self.has_next,
                    "hasPreviousPage": false,
                    "startCursor": self.nodes.first().map(|(_, cursor)| cursor),
                    "endCursor": self.nodes.last().map(|(_, cursor)| cursor),
                }),
            )),
            "totalCount" => R::leaf(self.total),
            name => {
                if let Some((_, typename, items)) =
                    self.lists.iter().find(|(field, ..)| *field == name)
                {
                    R::List(
                        items
                            .iter()
                            .map(|item| R::object(JsonObject::new(typename, item.clone())))
                            .collect(),
                    )
                } else {
                    // A specialized count its builder did not supply is a
                    // contract error, reported by non-null completion.
                    R::Leaf(self.scalars.get(name).cloned().unwrap_or(Value::Null))
                }
            }
        })
    }
}

struct Edge {
    typename: &'static str,
    node: Arc<dyn Object<Context>>,
    cursor: String,
}

#[async_trait]
impl Object<Context> for Edge {
    fn typename(&self) -> &str {
        self.typename
    }

    async fn resolve(&self, _: &Context, field: &Field<'_>) -> Result<R> {
        Ok(match field.name {
            "cursor" => R::leaf(self.cursor.clone()),
            _ => R::Object(Box::new(Shared(Arc::clone(&self.node)))),
        })
    }
}

pub(super) fn timestamp_seconds(seconds: u64) -> Value {
    let Ok(seconds) = i64::try_from(seconds) else {
        return Value::Null;
    };
    match time::OffsetDateTime::from_unix_timestamp(seconds) {
        Ok(time) => Value::String(format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            time.year(),
            u8::from(time.month()),
            time.day(),
            time.hour(),
            time.minute(),
            time.second()
        )),
        Err(_) => Value::Null,
    }
}

/// GitHub `DateTime` from a native millisecond timestamp, or null.
pub(super) fn timestamp(value: &Value) -> Value {
    value
        .as_u64()
        .map_or(Value::Null, |ms| timestamp_seconds(ms / 1000))
}

pub(super) fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

/// GitHub reports empty optional text as null.
pub(super) fn optional_text(value: &Value, key: &str) -> Value {
    match value[key].as_str() {
        Some("") | None => Value::Null,
        Some(text) => Value::String(text.to_owned()),
    }
}

/// A native list read gives one page per request; this joins pages until
/// the GitHub page is full, because native pages are smaller than GitHub's.
pub(super) const NATIVE_PAGE: usize = 50;

/// Bounds the native pages one GitHub request may read, which keeps
/// filtered listings and their counts finite on large repositories.
pub(super) const MAX_NATIVE_PAGES: usize = 20;
