use std::collections::BTreeSet;

use super::*;
use crate::cells::repository::push_attribution::{Authority, RefVersion, Verify, VerifyInput};

pub(super) struct Comparison {
    pub base: String,
    pub head: String,
    pub head_version: Option<RefVersion>,
}

pub(super) async fn comparison(
    repo: &Repository,
    pull: &PullRequest,
    fork: Option<&Repository>,
) -> Result<Option<Comparison>> {
    let mut names = BTreeSet::from([pull.base_ref.clone()]);
    let (base, head) = if let Some(fork) = fork {
        let head_names = BTreeSet::from([pull.head_ref.clone()]);
        let (base, head) = tokio::try_join!(
            repo.ref_view_for_refs(&names),
            fork.ref_view_for_refs(&head_names),
        )?;
        (base, Some(head))
    } else {
        names.insert(pull.head_ref.clone());
        (repo.ref_view_for_refs(&names).await?, None)
    };
    let head = head.as_ref().unwrap_or(&base);
    Ok(base.refs().get(&pull.base_ref).and_then(|base| {
        head.refs().get(&pull.head_ref).map(|oid| Comparison {
            base: base.clone(),
            head: oid.clone(),
            head_version: RefVersion::visible(head, &pull.head_ref),
        })
    }))
}

pub(super) async fn unchanged(
    source: &Repository,
    reference: &str,
    head: &TrackedHead,
) -> Result<bool> {
    let names = BTreeSet::from([reference.to_owned()]);
    let view = source.ref_view_for_refs(&names).await?;
    Ok(view.refs().get(reference) == Some(&head.oid)
        && RefVersion::visible(&view, reference).as_ref() == Some(&head.version))
}

pub(super) async fn follows(
    server: &Server,
    repo: &Repository,
    source: &Repository,
    reference: &str,
    previous: RefVersion,
    visible: RefVersion,
) -> Result<bool> {
    let routed = app::repository_cell(server, source)?;
    app::query_output(
        routed
            .client
            .query::<Verify>(
                &routed.target,
                None,
                VerifyInput {
                    reference: reference.to_owned(),
                    authority: Authority {
                        repository: repo.id,
                        ownership_epoch: repo.ownership_epoch,
                    },
                    previous,
                    visible,
                },
            )
            .await,
    )
}
