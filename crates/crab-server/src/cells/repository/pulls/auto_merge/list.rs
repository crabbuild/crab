use super::*;
use crate::cells::{Error, Result};

const LIMIT: usize = 16;

#[derive(Deserialize, Serialize)]
pub(crate) struct Query {
    pub branch: String,
    pub before: Option<String>,
    pub actor: RepositoryAuthor,
    pub can_write: bool,
    pub can_admin: bool,
}

#[derive(Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MergeQueueState"))]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryState {
    Waiting,
    Preparing,
    CandidateScheduled,
    Publishing,
}

#[derive(Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MergeQueueItem"))]
pub(crate) struct Entry {
    pull: u64,
    title: String,
    request_id: Uuid,
    head_oid: String,
    state: EntryState,
    can_remove: bool,
    can_prioritize: bool,
}

#[derive(Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MergeQueuePage"))]
pub(crate) struct Page {
    items: Vec<Entry>,
    next: Option<String>,
}

pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 440;
    type Input = Query;
    type Output = Page;

    fn execute(context: &mut Transaction<'_>, input: Query) -> Result<Page> {
        super::super::operations::validate_ref(&input.branch)?;
        validate_author(&input.actor)?;
        let prefix = queue_prefix(&input.branch);
        if let Some(before) = &input.before
            && !before.strip_prefix(&prefix).is_some_and(|key| {
                let key = key.strip_prefix("priority/").unwrap_or(key);
                key.len() == 16
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        {
            return Err(Error::Invalid(
                "Merge queue cursor is invalid for this branch",
            ));
        }

        let rows = context.documents::<u64>(&prefix, input.before.as_deref(), LIMIT + 1)?;
        let settings = settings::merge_methods::load_for_ref(context, &input.branch)?;
        let width = usize::from(settings.merge_queue_builds);
        let window = if input.before.is_none() {
            rows.iter()
                .take(width)
                .map(|(_, pull)| *pull)
                .collect::<Vec<_>>()
        } else {
            window::pulls(context, &input.branch, width)?
        };
        let order_revision = priority::revision(context, &input.branch)?;
        let first = window.first().copied();
        let can_reorder = if let Some(first) = first.filter(|_| input.can_admin) {
            let publishing = context.query(
                "SELECT pending_merge IS NOT NULL FROM repository_pulls WHERE number=?",
                &[integer(first)?],
            )?;
            publishing
                .rows
                .first()
                .map(|row| result_u64_from_row(row, 0))
                .transpose()?
                .is_some_and(|publishing| publishing == 0)
        } else {
            false
        };
        let next = (rows.len() > LIMIT).then(|| rows[LIMIT - 1].0.clone());
        let mut items = Vec::with_capacity(rows.len().min(LIMIT));
        for (position, number) in rows.into_iter().take(LIMIT) {
            let Some(job) = context.document::<Job>(&key(ACTIVE, number))? else {
                return Err(Error::Invalid("Merge queue entry has no active job"));
            };
            let Some(queue) = job.queue.filter(|queue| {
                job.base_ref == input.branch && queue_key(&job.base_ref, queue) == position
            }) else {
                return Err(Error::Invalid("Merge queue entry does not match its key"));
            };
            // A queue row only needs the current PR summary and authority.
            // Avoid loading descriptions, review decisions, and merge receipts.
            let pull = context.query(
                "SELECT title, version, author_issuer, author_subject, pending_merge IS NOT NULL
                 FROM repository_pulls WHERE number=?",
                &[integer(number)?],
            )?;
            let [row] = pull.rows.as_slice() else {
                return Err(Error::Invalid("Merge queue entry has no pull request"));
            };
            let version = result_u64_from_row(row, 1)?;
            let publishing = result_u64_from_row(row, 4)? != 0;
            let owns_pull = input.actor.issuer == result_text(row, 2)?
                && input.actor.subject == result_text(row, 3)?;
            let state = if publishing {
                EntryState::Publishing
            } else if !window.contains(&number) {
                EntryState::Waiting
            } else if queue.testing.is_some_and(|test| {
                test.version == version && test.order_revision == order_revision
            }) {
                EntryState::CandidateScheduled
            } else {
                EntryState::Preparing
            };
            items.push(Entry {
                pull: number,
                title: result_text(row, 0)?,
                request_id: job.id,
                head_oid: job.head_oid,
                state,
                can_remove: !publishing && (input.can_write || owns_pull),
                can_prioritize: can_reorder && !publishing && first != Some(number),
            });
        }
        Ok(Page { items, next })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRANCH: &str = "refs/heads/main";

    fn author() -> RepositoryAuthor {
        RepositoryAuthor {
            issuer: "https://identity.test".into(),
            subject: "writer".into(),
            name: "Writer".into(),
        }
    }

    async fn seed_queue(
        cell: &crate::cells::RepositoryCell,
        actor: &RepositoryAuthor,
        count: u64,
    ) -> Vec<Uuid> {
        let base_oid = "a".repeat(40);
        let head_oid = "b".repeat(40);
        for _ in 0..count {
            let opened = cell
                .client
                .command::<CreatePull>(
                    &cell.target,
                    crate::app::mutation_identity().unwrap(),
                    CreatePullInput {
                        code_owner_requests: Default::default(),
                        metadata: Default::default(),
                        draft: false,
                        submission_id: *Uuid::now_v7().as_bytes(),
                        author: actor.clone(),
                        title: "Queued change".into(),
                        body: String::new(),
                        base_ref: BRANCH.into(),
                        base_oid: base_oid.clone(),
                        head_ref: "refs/heads/feature".into(),
                        head_repository: None,
                        head_oid: head_oid.clone(),
                    },
                )
                .await
                .unwrap();
            assert!(matches!(opened.output, CreatePullOutcome::Created(_)));
        }
        let mut ids = Vec::new();
        for pull in (1..=count).rev() {
            let id = Uuid::now_v7();
            ids.push(id);
            let result = cell
                .client
                .command::<Edit>(
                    &cell.target,
                    crate::app::mutation_identity().unwrap(),
                    Change::Arm {
                        version: 1,
                        job: Job {
                            pull,
                            id,
                            actor: actor.clone(),
                            base_ref: BRANCH.into(),
                            head_ref: "refs/heads/feature".into(),
                            head_repository: None,
                            head_oid: head_oid.clone(),
                            initial_base_oid: base_oid.clone(),
                            initial_version: 1,
                            generation: 0,
                            queue_method_revision: 0,
                            comparison_revision: None,
                            method: MergeMethod::MergeCommit,
                            message: String::new(),
                            queue: None,
                        },
                        queue: true,
                        configured_queue_required: true,
                        ruleset_queue_required: false,
                    },
                )
                .await
                .unwrap();
            assert!(matches!(result.output, Outcome::Saved));
        }
        ids
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn branch_queue_pages_follow_admission_order_across_removals() {
        let server = crate::test_support::server::fixture().await;
        let repo = &server.repositories.values()[0];
        let cell = server.cells().unwrap().repository(repo.id).unwrap();
        let actor = author();
        let branch = BRANCH;
        let ids = seed_queue(&cell, &actor, LIMIT as u64 + 1).await;
        let read = async |branch: &str, before| {
            cell.client
                .query::<List>(
                    &cell.target,
                    None,
                    Query {
                        branch: branch.into(),
                        before,
                        actor: actor.clone(),
                        can_write: false,
                        can_admin: false,
                    },
                )
                .await
        };
        let first = read(branch, None).await.unwrap().output;
        assert_eq!(
            first
                .items
                .iter()
                .map(|entry| entry.pull)
                .collect::<Vec<_>>(),
            (2..=LIMIT as u64 + 1).rev().collect::<Vec<_>>()
        );
        assert!(matches!(first.items[0].state, EntryState::Preparing));
        assert!(matches!(first.items[1].state, EntryState::Waiting));
        assert!(first.items.iter().all(|entry| entry.can_remove));

        assert!(
            read("refs/heads/another", first.next.clone())
                .await
                .is_err()
        );
        let result = cell
            .client
            .command::<Edit>(
                &cell.target,
                crate::app::mutation_identity().unwrap(),
                Change::Disarm {
                    pull: 2,
                    id: ids[LIMIT - 1],
                    reason: Stop::Canceled,
                    front_revision: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(result.output, Outcome::Saved));
        let last = read(branch, first.next).await.unwrap().output;
        assert_eq!(
            last.items
                .iter()
                .map(|entry| entry.pull)
                .collect::<Vec<_>>(),
            [1]
        );
        assert!(last.next.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queue_build_window_reaches_one_hundred_entries() {
        use crate::cells::repository::{
            ReplaceMergeMethods, ReplaceMergeMethodsInput, ReplaceMergeMethodsOutcome,
        };

        let server = crate::test_support::server::fixture().await;
        let repo = &server.repositories.values()[0];
        let cell = server.cells().unwrap().repository(repo.id).unwrap();
        let actor = author();
        let configured = cell
            .client
            .command::<ReplaceMergeMethods>(
                &cell.target,
                crate::app::mutation_identity().unwrap(),
                ReplaceMergeMethodsInput {
                    expected_version: 0,
                    enabled: MergeMethod::ALL.to_vec(),
                    auto_merge: None,
                    delete_head_branches: None,
                    merge_queue: None,
                    merge_queue_method: None,
                    merge_queue_builds: Some(100),
                    merge_queue_timeout_minutes: None,
                    merge_queue_branches: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            configured.output,
            ReplaceMergeMethodsOutcome::Saved(_)
        ));

        let ids = seed_queue(&cell, &actor, 101).await;
        let get = async |pull| {
            cell.client
                .query::<Get>(&cell.target, None, pull)
                .await
                .unwrap()
                .output
                .unwrap()
        };
        let bounded = get(2).await;
        assert_eq!(
            bounded
                .ahead
                .unwrap()
                .into_iter()
                .map(|prior| prior.pull)
                .collect::<Vec<_>>(),
            (3..=101).rev().collect::<Vec<_>>(),
        );
        assert!(get(1).await.ahead.is_none());
        let test = async |pull: u64| {
            cell.client
                .command::<Edit>(
                    &cell.target,
                    crate::app::mutation_identity().unwrap(),
                    Change::Test {
                        pull,
                        id: ids[(101 - pull) as usize],
                        attempt: QueueTest {
                            request_id: Uuid::now_v7(),
                            version: 1,
                            base_oid: "a".repeat(40),
                            previous_oid: None,
                            order_revision: 0,
                        },
                        configured_queue_required: true,
                        ruleset_queue_required: false,
                    },
                )
                .await
                .unwrap()
                .output
        };
        assert!(matches!(test(2).await, Outcome::Saved));
        assert!(matches!(test(1).await, Outcome::Disabled));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn priority_and_begin_publication_fence_each_other() {
        let server = crate::test_support::server::fixture().await;
        let repo = &server.repositories.values()[0];
        let cell = server.cells().unwrap().repository(repo.id).unwrap();
        let actor = author();
        let ids = seed_queue(&cell, &actor, 2).await;
        let job = cell
            .client
            .query::<Get>(&cell.target, None, 2)
            .await
            .unwrap()
            .output
            .unwrap()
            .job;
        let commit_oid = "c".repeat(40);
        let merge = PullMerge {
            request_id: job
                .publication_id(1, &job.initial_base_oid, Some(&commit_oid), 0)
                .into_bytes(),
            author: actor.clone(),
            method: job.method,
            pull_version: 1,
            base_oid: job.initial_base_oid.clone(),
            head_oid: job.head_oid.clone(),
            commit_oid,
            message: String::new(),
            created_at_ms: 1,
            publication_plan: Some("d".repeat(64)),
            bypass_rulesets: Vec::new(),
            tested_candidate: true,
        };
        let transition = async |transition, merge: &PullMerge| {
            cell.client
                .command::<super::super::super::merge::TransitionPullMerge>(
                    &cell.target,
                    crate::app::mutation_identity().unwrap(),
                    TransitionPullMergeInput {
                        pull: 2,
                        merge: merge.clone(),
                        transition,
                    },
                )
                .await
                .unwrap()
                .output
        };
        let edit = async |change| {
            cell.client
                .command::<Edit>(
                    &cell.target,
                    crate::app::mutation_identity().unwrap(),
                    change,
                )
                .await
                .unwrap()
                .output
        };
        let prioritize = |pull: u64, request_id| {
            Change::Prioritize(Priority {
                pull,
                id: ids[(2 - pull) as usize],
                request_id,
                actor: actor.clone(),
                configured_queue_required: true,
                ruleset_queue_required: false,
            })
        };

        assert!(matches!(
            transition(PullMergeTransition::Begin, &merge).await,
            TransitionPullMergeOutcome::Applied(_)
        ));
        let request = Uuid::now_v7();
        assert!(matches!(
            edit(prioritize(1, request)).await,
            Outcome::MergePending
        ));
        let TransitionPullMergeOutcome::Applied(aborted) =
            transition(PullMergeTransition::Abort, &merge).await
        else {
            panic!("Expected the reserved publication to abort");
        };
        let stale = PullMerge {
            pull_version: aborted.version,
            request_id: job
                .publication_id(aborted.version, &merge.base_oid, Some(&merge.commit_oid), 0)
                .into_bytes(),
            ..merge
        };

        assert!(matches!(edit(prioritize(1, request)).await, Outcome::Saved));
        assert!(matches!(
            edit(prioritize(2, Uuid::now_v7())).await,
            Outcome::Saved
        ));
        // Even with the same pull back at the front, a worker holding the
        // original order cannot begin a merge or apply a failed-check decision.
        assert!(matches!(
            transition(PullMergeTransition::Begin, &stale).await,
            TransitionPullMergeOutcome::Conflict
        ));
        assert!(matches!(
            edit(Change::Disarm {
                pull: 2,
                id: ids[0],
                reason: Stop::ChecksFailed,
                front_revision: Some(0),
            })
            .await,
            Outcome::Changed
        ));
    }
}
