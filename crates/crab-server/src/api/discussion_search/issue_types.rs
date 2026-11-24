use super::*;
use crate::{
    api::organizations::issue_types::active,
    cells::{
        organization::issue_types::Schema,
        repository::{
            SearchInput,
            search::{Query as SearchQuery, completion, issue_types as native},
        },
    },
    repository::Repository,
};

pub(super) async fn options(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(input): Query<native::OptionInput>,
) -> Result<Json<native::Options>> {
    let repo = app::repository(&server, &principal, &key)?;
    let schema = active(&server, repo.organization).await?;
    let options = native::options(input, schema.definitions).map_err(|error| match error {
        crate::cells::Error::Invalid(message) => app::Error::Invalid(message),
        error => app::Error::Cell(error),
    })?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(options))
}

pub(in crate::api) async fn bind(
    server: &Server,
    repo: &Repository,
    input: &mut SearchInput,
) -> Result<Option<Schema>> {
    let Some(organization) = repo.organization else {
        return Ok(None);
    };
    let query = SearchQuery::parse(input.query.as_deref()).map_err(app::Error::Invalid)?;
    if !query.uses_issue_types() {
        return Ok(None);
    }
    let schema = active(server, Some(organization)).await?;
    input.issue_types = Some(native::Binding {
        organization,
        ownership_epoch: repo.ownership_epoch,
        version: schema.version,
        names: schema
            .definitions
            .iter()
            .map(|definition| (definition.name.to_lowercase(), definition.id))
            .collect(),
    });
    Ok(Some(schema))
}

pub(in crate::api) async fn verify(
    server: &Server,
    repo: &Repository,
    snapshot: Option<&Schema>,
) -> Result<()> {
    if let Some(snapshot) = snapshot
        && active(server, repo.organization).await?.version != snapshot.version
    {
        return Err(app::Error::Cell(crate::cells::Error::StaleSearch));
    }
    Ok(())
}

pub(super) async fn complete(
    server: &Server,
    repo: &Repository,
    input: &CompletionInput,
    result: &mut Suggestions,
) -> Result<()> {
    let Some(prefix) = completion::issue_type_prefix(input).map_err(app::Error::Invalid)? else {
        return Ok(());
    };
    let choices = active(server, repo.organization)
        .await?
        .definitions
        .into_iter()
        .filter(|definition| definition.name.to_lowercase().contains(&prefix))
        .map(|definition| {
            Ok((
                serde_json::to_string(&definition.name)?,
                definition.description,
            ))
        })
        .collect::<std::result::Result<Vec<_>, serde_json::Error>>()?;
    completion::append_qualified_values(input, result, "type", choices.into_iter())
        .map_err(app::Error::Invalid)
}
