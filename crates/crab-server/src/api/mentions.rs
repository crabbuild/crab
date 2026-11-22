use std::collections::BTreeSet;

use crate::{
    app::{self, Error, Result},
    cells::{
        Address, RepositoryCell,
        directory::{ResolveHandles, validate_handle},
        repository::collaboration::notifications::{Mentions, Recipient},
    },
    repository::Repository,
    server::Server,
};

const MAX_HANDLES: usize = 64;

pub(super) async fn cell<'a>(
    server: &Server,
    repo: &Repository,
    bodies: impl IntoIterator<Item = &'a str>,
) -> Result<RepositoryCell> {
    let mut cell = app::repository_cell(server, repo)?;
    let Some(auth) = &server.auth else {
        return Ok(cell);
    };
    let sources: Vec<_> = bodies
        .into_iter()
        .filter_map(|body| {
            let names = names(body);
            (!names.is_empty()).then(|| (blake3::hash(body.as_bytes()).to_hex().to_string(), names))
        })
        .collect();
    let handles: Vec<_> = sources
        .iter()
        .flat_map(|(_, names)| names.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_HANDLES)
        .collect();
    if handles.is_empty() {
        return Ok(cell);
    }
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let membership = catalog
        .membership(&repo.config.owner, &repo.config.name)
        .await
        .map_err(|error| Error::Membership(Box::new(error)))?;
    if membership.repository_id != repo.id || membership.organization != repo.organization {
        return Err(Error::NotFound);
    }
    let mut members: BTreeSet<_> = membership
        .members
        .into_iter()
        .map(|member| member.subject)
        .collect();
    if let Some(organization) = membership.organization {
        members.extend(
            crate::repository::access::organization_members(
                server,
                organization,
                &membership.teams,
                None,
            )
            .await?
            .into_iter()
            .map(|member| member.subject),
        );
    }
    let directory = server.cells().ok_or(Error::CellUnavailable)?;
    let resolved = app::query_output(
        directory
            .query::<ResolveHandles>(Address::Directory, None, handles.clone())
            .await,
    )?;
    if resolved.len() != handles.len() {
        return Err(Error::CellContract(
            "Mention lookup returned an invalid result count",
        ));
    }
    let recipients: std::collections::BTreeMap<_, _> = handles
        .into_iter()
        .zip(resolved)
        .filter_map(|(handle, matched)| {
            matched
                .filter(|account| {
                    account.issuer == auth.issuer() && members.contains(account.subject.as_str())
                })
                .map(|account| {
                    (
                        handle,
                        Recipient::qualified(&account.issuer, &account.subject),
                    )
                })
        })
        .collect();
    let groups = sources
        .into_iter()
        .filter_map(|(body_digest, names)| {
            let recipients: Vec<_> = names
                .iter()
                .filter_map(|name| recipients.get(name).copied())
                .collect();
            (!recipients.is_empty()).then_some(Mentions {
                body_digest,
                recipients,
            })
        })
        .collect();
    cell.client = cell.client.with_mentions(groups);
    Ok(cell)
}

fn names(body: &str) -> BTreeSet<String> {
    if !body.contains('@') {
        return BTreeSet::new();
    }
    let mut names = BTreeSet::new();
    for range in crate::markdown::prose_ranges(body) {
        // Inspect the original source so escapes and character
        // references cannot turn into executable mention syntax.
        let text = &body[range.clone()];
        for (offset, _) in text.match_indices('@') {
            let at = range.start + offset;
            if body[..at]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_alphanumeric() || "@_./\\-".contains(ch))
            {
                continue;
            }
            let tail = &text[offset + 1..];
            let length = tail
                .bytes()
                .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
                .count();
            if body[at + 1 + length..]
                .chars()
                .next()
                .is_some_and(|ch| ch.is_alphanumeric() || "/_@".contains(ch))
            {
                continue;
            }
            let handle = tail[..length].to_ascii_lowercase();
            if validate_handle(&handle).is_ok() {
                names.insert(handle);
            }
            if names.len() == MAX_HANDLES {
                return names;
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_require_markdown_prose_and_an_exact_handle() {
        let text = "Hello @Alice and **@bob**. @alice again. user@outside.test @team/dev @bad_name @chloé \\@escaped &#64;entity `@inline`\n\n> @quoted\n\n[link @link](https://example.test/@path)\n\n```txt\n@code\n```";
        assert_eq!(names(text), BTreeSet::from(["alice".into(), "bob".into()]));
    }
}
