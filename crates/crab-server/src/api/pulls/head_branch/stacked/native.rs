use super::*;

#[derive(Serialize)]
pub(crate) struct DeletePreview {
    pull: u64,
    version: u64,
    #[serde(flatten)]
    impact: Preview,
}

pub(crate) async fn preview(
    server: &Server,
    source: &Repository,
    grant: &app::Membership,
    reference: &str,
    head: &str,
) -> Result<Option<DeletePreview>> {
    let Some((parent, _, _)) = resolve(server, source, reference, head, None).await? else {
        return Ok(None);
    };
    let pull = associated_pull(server, source, source, parent).await?;
    let Some(branch) = check_availability(server, grant, source, &pull).await? else {
        return Err(Error::PullBranchInUse);
    };
    if branch.head_oid != head || !matches!(branch.action, Action::Delete) {
        return Err(Error::PullBranchChanged);
    }
    let impact = branch.retarget.ok_or(Error::PullBranchChanged)?;
    Ok(Some(DeletePreview {
        pull: parent,
        version: pull.version,
        impact,
    }))
}

async fn resolve(
    server: &Server,
    source: &Repository,
    reference: &str,
    head: &str,
    plan: Option<String>,
) -> Result<Option<(u64, String, cell::View)>> {
    let routed = app::repository_cell(server, source)?;
    match app::query_output(
        routed
            .client
            .query::<cell::Resolve>(
                &routed.target,
                None,
                cell::ResolveInput {
                    reference: reference.to_owned(),
                    old_oid: head.to_owned(),
                    plan,
                },
            )
            .await,
    )? {
        cell::Resolution::Unrelated => Ok(None),
        cell::Resolution::Ambiguous => Err(Error::StackedDeletionBlocked(
            "This branch was merged into different bases. Delete it from the intended merged pull request.",
        )),
        cell::Resolution::Stack { parent, base, view } => Ok(Some((parent, base, view))),
    }
}

pub(crate) struct Deletions(Vec<Deletion>);

struct Deletion {
    reference: String,
    head: String,
    plan: Option<String>,
    stack: Option<Stack>,
}

pub(crate) async fn plan(
    server: &Server,
    source: &Repository,
    updates: &[RefUpdate],
    wire_plan: Option<&str>,
) -> Result<Deletions> {
    let mut deletions = Vec::new();
    let mut dependents = 0;
    for update in updates {
        let Some(old) = update
            .old
            .filter(|_| update.new.is_none() && update.name.starts_with("refs/heads/"))
        else {
            continue;
        };
        let head = old.to_string();
        let plan = wire_plan.map(|id| {
            let mut digest = blake3::Hasher::new();
            digest.update(b"crab.native-stacked-deletion.v1\0");
            digest.update(id.as_bytes());
            digest.update(update.name.as_bytes());
            digest.finalize().to_hex().to_string()
        });
        let selection = resolve(server, source, &update.name, &head, plan.clone()).await?;
        let stack = if let Some((parent, base, view)) = selection {
            let plan = plan.clone().ok_or(Error::StackedDeletionBlocked(
                "Review the affected pull requests before deleting this branch.",
            ))?;
            let stack = build(
                server,
                source,
                Destination {
                    reference: update.name.clone(),
                    parent,
                    base,
                    plan,
                },
                view,
                None,
            )?
            .ok_or(Error::PullBranchChanged)?;
            if stack
                .local_refs()
                .any(|reference| updates.iter().any(|update| update.name == reference))
            {
                return Err(Error::StackedDeletionBlocked(
                    "Delete the stacked branch separately from changes to its replacement base or dependent heads.",
                ));
            }
            dependents += stack.dependents.len();
            if dependents > cell::LIMIT {
                return Err(Error::StackedDeletionBlocked(
                    "Delete fewer stacked branches per push. At most 16 dependent pull requests can move at once.",
                ));
            }
            Some(stack)
        } else {
            None
        };
        deletions.push(Deletion {
            reference: update.name.clone(),
            head,
            plan,
            stack,
        });
    }
    Ok(Deletions(deletions))
}

impl Deletions {
    pub(crate) fn stacks(&self) -> impl Iterator<Item = &Stack> {
        self.0.iter().filter_map(|deletion| deletion.stack.as_ref())
    }

    pub(crate) async fn apply(
        &self,
        server: &Server,
        source: &Repository,
        principal: &Principal,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let mut batches = Vec::new();
        for deletion in &self.0 {
            // The local ref lease prevents new base dependencies during this
            // check, the Cell transaction, and the subsequent Git publication.
            let current = resolve(
                server,
                source,
                &deletion.reference,
                &deletion.head,
                deletion.plan.clone(),
            )
            .await?;
            let stack = match (&deletion.stack, current) {
                (None, None) => continue,
                (Some(stack), Some((parent, base, _)))
                    if stack.destination.parent == parent && stack.destination.base == base =>
                {
                    stack
                }
                _ => return Err(Error::PullBranchChanged),
            };
            if in_use(server, source, &deletion.reference, true).await? {
                return Err(Error::PullBranchInUse);
            }
            if let Some(batch) = stack.prepare(server, source, principal, cancel).await? {
                batches.push(batch);
            }
        }
        persist(server, source, principal, batches, cancel).await?;
        for stack in self.stacks() {
            if in_use(server, source, &stack.destination.reference, false).await? {
                return Err(Error::PullBranchInUse);
            }
        }
        Ok(())
    }
}
