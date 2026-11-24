use super::*;

const PENDING: &str = "collaboration/v1/issue-transfers/pending/";
const WAKE: &str = "collaboration/v1/issue-transfers/wake/";
const RETRY_MS: i64 = 30_000;
#[cfg(not(test))]
pub(super) const IDLE_MS: i64 = 20 * 60 * 1000;
#[cfg(test)]
pub(super) const IDLE_MS: i64 = 12_000;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Work {
    Publish(u64),
    Expire(Uuid),
}

impl Work {
    fn key(self) -> String {
        match self {
            Self::Publish(number) => format!("publish/{number:016x}"),
            Self::Expire(request) => format!("expire/{request}"),
        }
    }
}

fn wake_key(at: i64, key: &str) -> String {
    format!("{WAKE}{at:016x}/{key}")
}

fn next(context: &Transaction<'_>) -> Result<Option<(i64, Work)>> {
    let rows = context.documents_after(WAKE, None, 1)?;
    Ok(rows.into_iter().next().map(|(_, wake)| wake))
}

fn schedule(context: &Transaction<'_>) -> Result<()> {
    context.set_alarm(next(context)?.map(|(at, _)| at))
}

pub(super) fn arm(context: &Transaction<'_>, work: Work, delay: i64) -> Result<()> {
    let key = work.key();
    let pending = format!("{PENDING}{key}");
    if let Some(at) = context.document::<i64>(&pending)? {
        context.delete_document(&wake_key(at, &key))?;
    }
    let at = context.now_ms().saturating_add(delay);
    context.put_document(&pending, &at)?;
    context.put_document(&wake_key(at, &key), &(at, work))?;
    schedule(context)
}

pub(super) fn finish(context: &Transaction<'_>, work: Work) -> Result<()> {
    let key = work.key();
    let pending = format!("{PENDING}{key}");
    if let Some(at) = context.document::<i64>(&pending)? {
        context.delete_document(&wake_key(at, &key))?;
        context.delete_document(&pending)?;
        schedule(context)?;
    }
    Ok(())
}

pub(super) fn due(context: &Transaction<'_>) -> Result<Option<Work>> {
    let Some((at, work)) = next(context)? else {
        context.set_alarm(None)?;
        return Ok(None);
    };
    if at > context.now_ms() {
        context.set_alarm(Some(at))?;
        return Ok(None);
    }
    if context.document::<i64>(&format!("{PENDING}{}", work.key()))? != Some(at) {
        return Err(Error::Invalid(
            "Issue transfer wake does not match pending work",
        ));
    }
    // Persist the retry before calling another Cell. Other due transfers stay
    // eligible if this transfer's peer is unreachable.
    arm(context, work, RETRY_MS)?;
    Ok(Some(work))
}
