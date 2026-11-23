use super::*;

const MAX_TRIES: u8 = 3;
const RETRY_MS: u64 = 60_000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Configuration {
    pub id: Uuid,
    pub installation: u64,
    pub repository: String,
    pub branch: String,
}

impl Configuration {
    pub(crate) fn valid(&self) -> bool {
        let Some((owner, repository)) = self.repository.split_once('/') else {
            return false;
        };
        !self.id.is_nil()
            && validate_number(self.installation).is_ok()
            && [owner, repository].into_iter().all(|part| {
                !part.is_empty()
                    && part.len() <= 100
                    && !matches!(part, "." | "..")
                    && part
                        .bytes()
                        .all(|ch| ch.is_ascii_alphanumeric() || b"_.-".contains(&ch))
            })
            && !self.branch.starts_with("refs/")
            && self.branch.len() <= 255
            && crab_git::validate_push_refname(&format!("refs/heads/{}", self.branch)).is_ok()
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Key {
    pub connection: Uuid,
    pub oid: String,
}

impl Key {
    fn document(&self, generation: Uuid) -> String {
        format!(
            "ci/v1/dispatch/{}/{generation}/{}",
            self.connection, self.oid
        )
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Dispatch {
    pub id: Uuid,
    pub pull: u64,
    pub credential: Uuid,
    pub generation: Uuid,
    pub run: Option<u64>,
    pub failed: bool,
    pub attempts: u8,
    retry_at: u64,
}

impl Dispatch {
    pub(crate) fn ready(&self, now: u64) -> bool {
        self.run.is_none() && !self.failed && now >= self.retry_at
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Current {
    pub connection: Connection,
    pub dispatch: Option<Dispatch>,
}

fn load(context: &Transaction<'_>, key: &Key) -> crate::cells::Result<Option<Current>> {
    checks::validate_commit(&key.oid)?;
    let Some(connection) = connections(context)?
        .into_iter()
        .find(|row| row.id == key.connection)
    else {
        return Ok(None);
    };
    let Some(config) = &connection.dispatch else {
        return Ok(None);
    };
    let dispatch = context
        .document::<Dispatch>(&key.document(config.id))?
        .filter(|dispatch| dispatch.credential == connection.credential);
    Ok(Some(Current {
        connection,
        dispatch,
    }))
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 421;
    type Input = Key;
    type Output = Option<Current>;

    fn execute(context: &mut Transaction<'_>, key: Key) -> crate::cells::Result<Self::Output> {
        load(context, &key)
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Start {
    pub key: Key,
    pub id: Uuid,
    pub pull: u64,
    pub credential: Uuid,
    pub generation: Uuid,
}

pub(crate) struct Reserve;
impl Mutation for Reserve {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 421;
    type Input = Start;
    type Output = Option<Dispatch>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Start,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        use MutationResult::{Rejected, Success};
        validate_number(input.pull)?;
        let Some(current) = load(context, &input.key)? else {
            return Ok(Rejected(None));
        };
        if settings::archived(context)?
            || current.connection.credential != input.credential
            || current.connection.dispatch.as_ref().map(|config| config.id)
                != Some(input.generation)
            || input.id.is_nil()
        {
            return Ok(Rejected(None));
        }
        let now = timestamp(context.now_ms())?;
        let mut dispatch = current.dispatch.unwrap_or(Dispatch {
            id: input.id,
            pull: input.pull,
            credential: input.credential,
            generation: input.generation,
            run: None,
            failed: false,
            attempts: 0,
            retry_at: 0,
        });
        if dispatch.pull != input.pull || !dispatch.ready(now) {
            return Ok(Rejected(None));
        }
        // Record the network attempt before sending it. A workflow report can
        // recover the run even when GitHub's reply never reaches the worker.
        if dispatch.attempts >= MAX_TRIES {
            dispatch.failed = true;
            context.put_document(&input.key.document(dispatch.generation), &dispatch)?;
            return Ok(Success(None));
        }
        dispatch.attempts += 1;
        dispatch.retry_at = now.saturating_add(RETRY_MS);
        context.put_document(&input.key.document(dispatch.generation), &dispatch)?;
        Ok(Success(Some(dispatch)))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Finish {
    pub key: Key,
    pub id: Uuid,
    pub run: Option<u64>,
}

pub(crate) struct Confirm;
impl Mutation for Confirm {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 422;
    type Input = Finish;
    type Output = bool;

    fn execute(
        context: &mut Transaction<'_>,
        input: Finish,
    ) -> crate::cells::Result<MutationResult<bool>> {
        let saved = match input.run {
            Some(run) => bind(context, &input.key, input.id, run)?.is_some(),
            None => {
                if let Some(mut dispatch) = matching(context, &input.key, input.id)? {
                    if dispatch.run.is_none() {
                        dispatch.failed = true;
                        context
                            .put_document(&input.key.document(dispatch.generation), &dispatch)?;
                    }
                    true
                } else {
                    false
                }
            }
        };
        Ok(if saved {
            MutationResult::Success(true)
        } else {
            MutationResult::Rejected(false)
        })
    }
}

fn matching(
    context: &Transaction<'_>,
    key: &Key,
    id: Uuid,
) -> crate::cells::Result<Option<Dispatch>> {
    Ok(load(context, key)?
        .and_then(|current| current.dispatch)
        .filter(|dispatch| dispatch.id == id))
}

pub(super) fn bind(
    context: &Transaction<'_>,
    key: &Key,
    id: Uuid,
    run: u64,
) -> crate::cells::Result<Option<Uuid>> {
    validate_number(run)?;
    let Some(mut dispatch) = matching(context, key, id)? else {
        return Ok(None);
    };
    if dispatch.run.is_some_and(|bound| bound != run) {
        return Ok(None);
    }
    if dispatch.run.is_none() {
        dispatch.run = Some(run);
        dispatch.failed = false;
        context.put_document(&key.document(dispatch.generation), &dispatch)?;
    }
    Ok(Some(dispatch.generation))
}
