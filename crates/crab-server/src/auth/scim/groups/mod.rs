use std::collections::BTreeSet;

use super::*;
use crate::{app, cells::organization::teams::sync::MAX_GROUP_MEMBERS};

mod store;

use store::{Group, Groups, TeamTarget};

pub(super) const GROUP: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const PATCH: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/Groups", post(create).get(list))
        .route(
            "/Groups/{id}",
            get(read).put(replace).patch(patch).delete(delete),
        )
        .layer(DefaultBodyLimit::max(128 * 1024))
}

pub(crate) fn enabled(server: &Server) -> bool {
    server
        .auth
        .as_ref()
        .is_some_and(|auth| auth.scim_key.is_some())
}

pub(crate) async fn configure_team(
    server: &Server,
    organization: Uuid,
    team: Uuid,
    actor: Uuid,
    version: u64,
    group: Option<Uuid>,
) -> app::Result<()> {
    if group.is_some() && !enabled(server) {
        return Err(app::Error::Invalid(
            "Enable SCIM provisioning before linking an identity provider group",
        ));
    }
    let target = TeamTarget { organization, team };
    let directory = Groups::new(server).map_err(link_error)?;
    directory
        .link(target, actor, version, group)
        .await
        .map_err(link_error)
}

fn link_error(error: Error) -> app::Error {
    match error.0 {
        StatusCode::CONFLICT => app::Error::Conflict,
        StatusCode::NOT_FOUND => app::Error::Invalid("Enter an active SCIM group ID"),
        StatusCode::INSUFFICIENT_STORAGE => {
            app::Error::Invalid("The SCIM directory has reached its group binding limit")
        }
        _ => app::Error::CellUnavailable,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupInput {
    schemas: Vec<String>,
    display_name: String,
    external_id: Option<String>,
    #[serde(default)]
    members: Vec<Member>,
}

#[derive(Deserialize)]
struct Member {
    value: Uuid,
    #[serde(rename = "type")]
    kind: Option<String>,
}

impl GroupInput {
    fn validate(&self) -> Result<BTreeSet<Uuid>> {
        if !self.schemas.iter().any(|schema| schema == GROUP) {
            return Err(Error::invalid(
                "invalidSyntax",
                "The core Group schema is required",
            ));
        }
        validate_group_name(&self.display_name)?;
        if self
            .external_id
            .as_deref()
            .is_some_and(|id| !valid_subject(id))
        {
            return Err(Error::invalid("invalidValue", "Invalid group externalId"));
        }
        member_ids(&self.members)
    }
}

fn validate_group_name(name: &str) -> Result<()> {
    if !(1..=256).contains(&name.chars().count())
        || name.trim() != name
        || name.chars().any(char::is_control)
    {
        return Err(Error::invalid(
            "invalidValue",
            "A valid group displayName is required",
        ));
    }
    Ok(())
}

fn member_ids(members: &[Member]) -> Result<BTreeSet<Uuid>> {
    if members.len() > MAX_GROUP_MEMBERS
        || members.iter().any(|member| {
            member.value.is_nil()
                || member
                    .kind
                    .as_ref()
                    .is_some_and(|kind| !kind.eq_ignore_ascii_case("User"))
        })
    {
        return Err(Error::invalid(
            "invalidValue",
            "Group members must be provisioned Users",
        ));
    }
    Ok(members.iter().map(|member| member.value).collect())
}

fn document(auth: &Authentication, group: &Group) -> Value {
    let mut value = json!({
        "schemas":[GROUP],"id":group.id,"displayName":group.name,
        "members":group.members.iter().map(|id| {
            json!({"value":id,"type":"User","$ref":super::location(auth, *id)})
        }).collect::<Vec<_>>(),
        "meta":{
            "resourceType":"Group","created":group.created,"lastModified":group.updated,
            "location":group_location(auth,group.id),"version":group.tag(),
        },
    });
    if let Some(id) = &group.external_id {
        value["externalId"] = json!(id);
    }
    value
}

fn group_location(auth: &Authentication, id: Uuid) -> String {
    format!(
        "{}{ROOT}/Groups/{id}",
        auth.config.public_url.as_str().trim_end_matches('/')
    )
}

fn group_response(status: StatusCode, auth: &Authentication, group: &Group) -> Response {
    let mut result = response(status, document(auth, group));
    if let Ok(tag) = group.tag().parse() {
        result.headers_mut().insert(header::ETAG, tag);
    }
    if status == StatusCode::CREATED
        && let Ok(location) = group_location(auth, group.id).parse()
    {
        result.headers_mut().insert(header::LOCATION, location);
    }
    result
}

enum Filter {
    Name(String),
    ExternalId(String),
}
impl Filter {
    fn parse(expression: &str) -> Result<Self> {
        let invalid = || {
            Error::invalid(
                "invalidFilter",
                "Use displayName eq or externalId eq with a quoted value",
            )
        };
        let (attr, rest) = expression
            .trim()
            .split_once(char::is_whitespace)
            .ok_or_else(invalid)?;
        let (op, value) = rest
            .trim_start()
            .split_once(char::is_whitespace)
            .ok_or_else(invalid)?;
        if !op.eq_ignore_ascii_case("eq") {
            return Err(invalid());
        }
        let value: String = serde_json::from_str(value.trim()).map_err(|_| invalid())?;
        match attr.to_ascii_lowercase().as_str() {
            "displayname" => Ok(Self::Name(value.to_lowercase())),
            "externalid" => Ok(Self::ExternalId(value)),
            _ => Err(invalid()),
        }
    }

    fn matches(&self, group: &Group) -> bool {
        match self {
            Self::Name(name) => group.name.to_lowercase() == *name,
            Self::ExternalId(id) => group.external_id.as_ref() == Some(id),
        }
    }
}

async fn list(
    State(server): State<Arc<Server>>,
    params: std::result::Result<Query<ListParameters>, QueryRejection>,
) -> Result<Response> {
    let Query(params) =
        params.map_err(|_| Error::invalid("invalidValue", "Invalid query parameters"))?;
    if params.sort_by.is_some() {
        return Err(Error::invalid(
            "invalidValue",
            "SCIM sorting is unavailable",
        ));
    }
    let filter = params.filter.as_deref().map(Filter::parse).transpose()?;
    let start = params.start_index.unwrap_or(1).max(1) as usize;
    let count = params.count.unwrap_or(100).clamp(0, 100) as usize;
    let groups = Groups::new(&server)?;
    let values = groups.list().await?;
    let mut eligible = values.values().filter(|group| {
        !group.deleted && filter.as_ref().is_none_or(|filter| filter.matches(group))
    });
    let total = eligible.clone().count();
    let resources = eligible
        .nth(start - 1)
        .into_iter()
        .chain(eligible)
        .take(count)
        .map(|group| document(groups.auth, group))
        .collect();
    Ok(list_response(resources, total, start))
}

async fn read(State(server): State<Arc<Server>>, Path(id): Path<Uuid>) -> Result<Response> {
    let groups = Groups::new(&server)?;
    let values = groups.list().await?;
    let group = values
        .get(&id)
        .filter(|group| !group.deleted)
        .ok_or_else(Error::not_found)?;
    Ok(group_response(StatusCode::OK, groups.auth, group))
}

fn unique_name<'a>(
    groups: impl Iterator<Item = &'a Group>,
    name: &str,
    except: Uuid,
) -> Result<()> {
    let name = name.to_lowercase();
    if groups
        .filter(|group| group.id != except && !group.deleted)
        .any(|group| group.name.to_lowercase() == name)
    {
        return Err(Error::conflict("Group displayName is already assigned"));
    }
    Ok(())
}

async fn create(
    State(server): State<Arc<Server>>,
    input: std::result::Result<Json<GroupInput>, JsonRejection>,
) -> Result<Response> {
    let input = body(input)?;
    let members = input.validate()?;
    let groups = Groups::new(&server)?;
    let entry = groups
        .update(|values| {
            let previous = input.external_id.as_ref().and_then(|id| {
                values
                    .values()
                    .find(|group| group.external_id.as_ref() == Some(id))
            });
            if previous.is_some_and(|group| !group.deleted) {
                return Err(Error::conflict("Group externalId is already assigned"));
            }
            let id = previous.map_or_else(Uuid::now_v7, |group| group.id);
            unique_name(values.values(), &input.display_name, id)?;
            let updated = timestamp()?;
            let group = Group {
                id,
                external_id: input.external_id.clone(),
                name: input.display_name.clone(),
                members: members.clone(),
                revision: previous.map_or(Ok(1), |group| next(group.revision))?,
                created: previous.map_or_else(|| updated.clone(), |group| group.created.clone()),
                updated,
                deleted: false,
                links: previous.map_or_else(Vec::new, |old| old.links.clone()),
            };
            values.insert(id, group);
            Ok(id)
        })
        .await?;
    Ok(group_response(StatusCode::CREATED, groups.auth, &entry))
}

fn next(revision: u64) -> Result<u64> {
    revision.checked_add(1).ok_or_else(Error::capacity)
}

async fn update_group(
    groups: &Groups<'_>,
    id: Uuid,
    headers: &HeaderMap,
    change: impl Fn(&mut Group) -> Result<()>,
) -> Result<Group> {
    groups
        .update(|values| {
            let mut group = values
                .remove(&id)
                .filter(|group| !group.deleted)
                .ok_or_else(Error::not_found)?;
            if headers
                .get(header::IF_MATCH)
                .is_some_and(|tag| tag != "*" && tag != group.tag().as_str())
            {
                return Err(Error(
                    StatusCode::PRECONDITION_FAILED,
                    None,
                    "SCIM resource version changed",
                ));
            }
            change(&mut group)?;
            if !group.deleted {
                validate_group_name(&group.name)?;
                unique_name(values.values(), &group.name, id)?;
                if group.members.len() > MAX_GROUP_MEMBERS {
                    return Err(Error::capacity());
                }
            }
            group.updated = timestamp()?;
            group.revision = next(group.revision)?;
            values.insert(id, group);
            Ok(id)
        })
        .await
}

async fn replace(
    State(server): State<Arc<Server>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    input: std::result::Result<Json<GroupInput>, JsonRejection>,
) -> Result<Response> {
    let input = body(input)?;
    let members = input.validate()?;
    let groups = Groups::new(&server)?;
    let group = update_group(&groups, id, &headers, |group| {
        if input.external_id.is_some() && input.external_id != group.external_id {
            return Err(Error::invalid(
                "mutability",
                "Group externalId is immutable",
            ));
        }
        group.name.clone_from(&input.display_name);
        group.members.clone_from(&members);
        Ok(())
    })
    .await?;
    Ok(group_response(StatusCode::OK, groups.auth, &group))
}

async fn delete(
    State(server): State<Arc<Server>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response> {
    let groups = Groups::new(&server)?;
    update_group(&groups, id, &headers, |group| {
        group.members.clear();
        group.deleted = true;
        Ok(())
    })
    .await?;
    Ok((
        StatusCode::NO_CONTENT,
        [(header::CACHE_CONTROL, "no-store")],
    )
        .into_response())
}

#[derive(Deserialize)]
struct GroupPatch {
    schemas: Vec<String>,
    #[serde(rename = "Operations")]
    operations: Vec<Operation>,
}

#[derive(Deserialize)]
struct Operation {
    op: String,
    path: Option<String>,
    #[serde(default)]
    value: Value,
}

impl GroupPatch {
    fn apply(&self, group: &mut Group) -> Result<()> {
        if !self.schemas.iter().any(|schema| schema == PATCH)
            || !(1..=32).contains(&self.operations.len())
        {
            return Err(Error::invalid(
                "invalidSyntax",
                "A PatchOp with 1 to 32 operations is required",
            ));
        }
        for operation in &self.operations {
            let op = operation.op.to_ascii_lowercase();
            match operation.path.as_deref() {
                Some(path) => edit_field(group, &op, path, &operation.value)?,
                None if matches!(op.as_str(), "add" | "replace") => {
                    let fields = operation.value.as_object().ok_or_else(invalid_patch)?;
                    for (path, value) in fields {
                        edit_field(group, &op, path, value)?;
                    }
                }
                _ => return Err(invalid_patch()),
            }
        }
        Ok(())
    }
}

fn invalid_patch() -> Error {
    Error::invalid("invalidValue", "Invalid Group patch operation")
}

fn edit_field(group: &mut Group, op: &str, path: &str, value: &Value) -> Result<()> {
    let path = path.to_ascii_lowercase();
    let path = path
        .strip_prefix("urn:ietf:params:scim:schemas:core:2.0:group:")
        .unwrap_or(&path);
    match (op, path) {
        ("add" | "replace", "displayname") => {
            group.name = value.as_str().ok_or_else(invalid_patch)?.into();
        }
        ("add" | "replace", "members") => {
            let members: Vec<Member> =
                serde_json::from_value(value.clone()).map_err(|_| invalid_patch())?;
            let ids = member_ids(&members)?;
            if op == "replace" {
                group.members = ids;
            } else {
                group.members.extend(ids);
            }
        }
        ("remove", "members") => {
            if value.is_null() {
                group.members.clear();
            } else {
                let members: Vec<Member> =
                    serde_json::from_value(value.clone()).map_err(|_| invalid_patch())?;
                group
                    .members
                    .retain(|member| !members.iter().any(|entry| entry.value == *member));
            }
        }
        ("remove", path) if path.starts_with("members[") => {
            let filter = path
                .strip_prefix("members[")
                .and_then(|value| value.strip_suffix(']'))
                .ok_or_else(invalid_patch)?;
            let value = filter
                .trim()
                .strip_prefix("value")
                .map(str::trim_start)
                .and_then(|value| value.strip_prefix("eq"))
                .map(str::trim_start)
                .ok_or_else(invalid_patch)?;
            let member: Uuid = serde_json::from_str(value).map_err(|_| invalid_patch())?;
            group.members.remove(&member);
        }
        (_, "id" | "externalid" | "meta") => {
            return Err(Error::invalid(
                "mutability",
                "The selected attribute is immutable",
            ));
        }
        _ => return Err(invalid_patch()),
    }
    Ok(())
}

async fn patch(
    State(server): State<Arc<Server>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    input: std::result::Result<Json<GroupPatch>, JsonRejection>,
) -> Result<Response> {
    let input = body(input)?;
    let groups = Groups::new(&server)?;
    let group = update_group(&groups, id, &headers, |group| input.apply(group)).await?;
    Ok(group_response(StatusCode::OK, groups.auth, &group))
}
