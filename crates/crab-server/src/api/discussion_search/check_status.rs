use std::collections::{BTreeMap, BTreeSet};

use futures_util::{StreamExt, TryStreamExt, stream};

use crate::{
    app::{self, Error, Result},
    cells::repository::{
        SearchInput,
        search::check_status::{
            Context, ForkRefInput, ListForkRefs, MAX_FORK_HEAD_BYTES, fork_ref,
        },
    },
    repository::Repository,
    server::Server,
};

pub(super) async fn bind(
    server: &Server,
    repo: &Repository,
    input: &mut SearchInput,
) -> Result<()> {
    let cell = app::repository_cell(server, repo)?;
    let mut after = 0;
    let mut stamp = None;
    let mut forks = BTreeMap::<_, BTreeSet<_>>::new();
    let stamp = loop {
        let page = app::query_output(
            cell.client
                .query::<ListForkRefs>(&cell.target, None, ForkRefInput { after, stamp })
                .await,
        )?;
        for reference in page.refs {
            let (fork, name) = fork_ref(&reference)
                .map_err(|_| Error::CellContract("Invalid qualified fork ref"))?;
            forks.entry(fork).or_default().insert(name.to_owned());
        }
        let Some(next) = page.next else {
            break page.stamp;
        };
        after = next;
        stamp = Some(page.stamp);
    };
    let source_token = match &input.review_context {
        Some(review) => review.source_token.clone(),
        None => super::reviews::live_source(server, repo).await?.0,
    };
    let mut snapshots = stream::iter(forks)
        .map(|(id, refs)| async move {
            let tips = match crate::api::pulls::git::fork_repository(server, repo, id) {
                Ok(fork) => fork.ref_oids(&refs).await?,
                Err(Error::NotFound) => return Ok(None),
                Err(error) => return Err(error),
            };
            Ok(Some((id, tips)))
        })
        .buffer_unordered(6);
    let mut fork_heads = BTreeMap::new();
    let mut encoded_bytes = 2;
    while let Some(snapshot) = snapshots.try_next().await? {
        let Some((fork, tips)) = snapshot else {
            continue;
        };
        for (reference, oid) in tips {
            let qualified = format!("{fork}:{reference}");
            encoded_bytes += serde_json::to_string(&qualified)?.len() + oid.len() + 4;
            if encoded_bytes > MAX_FORK_HEAD_BYTES {
                return Err(Error::Invalid(
                    "Too many live fork branches to search check status",
                ));
            }
            fork_heads.insert(qualified, oid);
        }
    }
    input.check_context = Some(Context {
        source_token,
        stamp,
        fork_heads,
    });
    Ok(())
}
