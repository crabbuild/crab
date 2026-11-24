use super::*;
use crate::cells::repository::collaboration::{
    activity::Action,
    attachments::{self, Upload},
};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Edit {
    pub(crate) number: u64,
    previous_retained: bool,
    redaction: Option<Redaction>,
    body: Option<String>,
    pub(crate) attachments: Vec<Upload>,
}

impl Edit {
    pub(in crate::cells::repository) fn validate(&self, source: Uuid, event: &Event) -> Result<()> {
        if self.number != event.number
            || !matches!(event.action, Action::Updated)
            || !matches!(
                event.target,
                Target::Issue { .. } | Target::IssueComment { .. }
            )
        {
            return Err(invalid());
        }
        match (&self.redaction, &self.body) {
            (Some(redaction), None) if self.attachments.is_empty() => {
                validate_author(&redaction.author)?;
            }
            (None, Some(body)) => {
                crate::cells::repository::validate_body(body, false)?;
                attachments::validate_transfer(source, body, &self.attachments)?;
            }
            _ => return Err(invalid()),
        }
        Ok(())
    }
}

fn invalid() -> Error {
    Error::Invalid("Issue transfer edit history is invalid")
}

pub(in crate::cells::repository) fn capture(
    context: &Transaction<'_>,
    source: Uuid,
    event: &Event,
) -> Result<Option<Edit>> {
    if !matches!(event.action, Action::Updated) {
        return Ok(None);
    }
    let history = History::new(event.target);
    let Some(revision) = context.document::<Revision>(&history.revision(event.number))? else {
        return Ok(None);
    };
    if revision.number != event.number
        || revision.edited_at != event.created_at
        || !same_author(&revision.editor, &event.actor)
    {
        return Err(invalid());
    }
    let previous_retained = if let Some(previous) = revision.previous {
        validate_number(previous)?;
        if previous >= revision.number {
            return Err(invalid());
        }
        context
            .document::<Revision>(&history.revision(previous))?
            .is_some()
    } else {
        false
    };
    let body = if let Some(content) = history.content(context, &revision)? {
        if content.suggestion.is_some() {
            return Err(invalid());
        }
        Some(content.body)
    } else {
        None
    };
    let attachments = body
        .as_deref()
        .map(|value| attachments::capture_transfer(context, source, value))
        .transpose()?
        .unwrap_or_default();
    let edit = Edit {
        number: revision.number,
        previous_retained,
        redaction: revision.redaction,
        body,
        attachments,
    };
    edit.validate(source, event)?;
    Ok(Some(edit))
}

pub(in crate::cells::repository) fn install(
    context: &Transaction<'_>,
    request: Uuid,
    source: Uuid,
    destination: Uuid,
    event: &Event,
    edit: Edit,
) -> Result<()> {
    let history = History::new(event.target);
    let mut state = context
        .document::<Retention>(&history.retention())?
        .unwrap_or(Retention {
            original: event.number,
            latest: 0,
            count: 0,
        });
    if state.count >= LIMIT
        || state.latest >= event.number
        || (edit.previous_retained && state.latest == 0)
    {
        return Err(invalid());
    }
    // Activity is imported oldest first. An adjacent source revision maps to
    // the latest retained destination revision. A pruned gap must stay a gap.
    let revision = Revision {
        number: event.number,
        editor: event.actor.clone(),
        edited_at: event.created_at,
        redaction: edit.redaction,
        previous: edit.previous_retained.then_some(state.latest),
    };
    if let Some(mut body) = edit.body {
        attachments::publish_transfer(
            context,
            request,
            source,
            destination,
            event.target,
            &mut body,
        )?;
        history.store_content(context, event.number, &body, None)?;
    }
    context.put_document(&history.revision(event.number), &revision)?;
    state.latest = event.number;
    state.count += 1;
    context.put_document(&history.retention(), &state)
}
