use crate::{
    app::{self, Result},
    auth::Principal,
    cells::{
        Address,
        directory::ResolveHandles,
        repository::{
            SearchInput,
            search::{
                Query,
                authors::{self, Binding},
            },
        },
    },
    server::Server,
};

pub(in crate::api) async fn resolve(
    server: &Server,
    principal: &Principal,
    input: &mut SearchInput,
) -> Result<()> {
    let query = Query::parse(input.query.as_deref()).map_err(app::Error::Invalid)?;
    let names = query.authors();
    if names.is_empty() {
        input.authors.clear();
        return Ok(());
    }
    let handles: Vec<_> = names
        .iter()
        .filter(|name| name.as_str() != "@me")
        .cloned()
        .collect();
    let resolved = if handles.is_empty() {
        Vec::new()
    } else {
        let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
        app::query_output(
            cells
                .query::<ResolveHandles>(Address::Directory, None, handles.clone())
                .await,
        )?
    };
    if resolved.len() != handles.len() {
        return Err(app::Error::CellContract(
            "Author resolution returned an invalid result count",
        ));
    }
    let mut by_handle: std::collections::BTreeMap<_, _> =
        handles.into_iter().zip(resolved).collect();
    let actor = app::actor(principal)?;
    input.authors = names
        .into_iter()
        .map(|name| {
            let identity = if name == "@me" {
                Some(authors::identity(&actor.issuer, &actor.subject).map_err(app::Error::Cell)?)
            } else {
                by_handle
                    .remove(&name)
                    .flatten()
                    .map(|key| authors::identity(&key.issuer, &key.subject))
                    .transpose()
                    .map_err(app::Error::Cell)?
            };
            Ok(Binding { name, identity })
        })
        .collect::<Result<_>>()?;
    Ok(())
}

#[cfg(test)]
mod tests;
