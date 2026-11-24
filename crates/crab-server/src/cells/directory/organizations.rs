use super::*;

const PREFIX: &str = "directory/v1/organization-candidates/";
pub(crate) const PAGE_SIZE: usize = 40;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Candidate {
    pub actor: Uuid,
    pub organization: Uuid,
}

fn key(candidate: &Candidate) -> String {
    let digest = blake3::derive_key(
        "crab organization candidate v1",
        &[
            candidate.actor.as_bytes().as_slice(),
            candidate.organization.as_bytes(),
        ]
        .concat(),
    );
    let hash = blake3::Hash::from_bytes(digest);
    format!("{PREFIX}{}/{hash}", candidate.actor.simple())
}

pub(crate) struct RecordCandidate;
impl Mutation for RecordCandidate {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 80;
    type Input = Candidate;
    type Output = ();

    fn execute(
        context: &mut Transaction<'_>,
        candidate: Self::Input,
    ) -> Result<MutationResult<()>> {
        if candidate.actor.is_nil() || candidate.organization.is_nil() {
            return Err(Error::Invalid("Invalid organization candidate"));
        }
        let key = key(&candidate);
        if context.document::<Candidate>(&key)?.is_none() {
            context.put_document(&key, &candidate)?;
        }
        Ok(MutationResult::Success(()))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub actor: Option<Uuid>,
    pub after: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Page {
    pub items: Vec<Candidate>,
    pub next: Option<String>,
}

pub(crate) struct Candidates;
impl Read for Candidates {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 80;
    type Input = Query;
    type Output = Page;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Page> {
        let prefix = input.actor.map_or_else(
            || PREFIX.to_owned(),
            |actor| format!("{PREFIX}{}/", actor.simple()),
        );
        let after = input
            .after
            .map(|cursor| {
                let valid = if input.actor.is_some() {
                    blake3::Hash::from_hex(&cursor).is_ok()
                } else {
                    cursor.split_once('/').is_some_and(|(actor, digest)| {
                        actor.len() == 32
                            && Uuid::parse_str(actor).is_ok()
                            && blake3::Hash::from_hex(digest).is_ok()
                    })
                };
                if !valid {
                    return Err(Error::Invalid("Invalid organization continuation"));
                }
                Ok(format!("{prefix}{cursor}"))
            })
            .transpose()?;
        let mut rows =
            context.documents_after::<Candidate>(&prefix, after.as_deref(), PAGE_SIZE + 1)?;
        let next = if rows.len() > PAGE_SIZE {
            rows.pop();
            rows.last()
                .and_then(|(key, _)| key.strip_prefix(&prefix))
                .map(str::to_owned)
        } else {
            None
        };
        Ok(Page {
            items: rows.into_iter().map(|(_, entry)| entry).collect(),
            next,
        })
    }
}
