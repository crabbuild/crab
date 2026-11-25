use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cells::{
    Result,
    application::{Mutation, MutationResult, Read, Transaction},
};

const MAX_TRANSITIONS: usize = 128;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RefVersion {
    pub epoch: String,
    pub transaction: String,
}

impl RefVersion {
    pub(crate) fn visible(
        view: &crab_read::capsule_protocol::CapsuleRefView,
        reference: &str,
    ) -> Option<Self> {
        Some(Self {
            epoch: view.root_snapshot().record().root().ref_epoch().to_owned(),
            transaction: view.visible_ref_transactions().get(reference)?.clone(),
        })
    }
}

pub(crate) type Actor = [u8; 32];

pub(crate) fn actor(issuer: &str, subject: &str) -> Actor {
    let mut digest = blake3::Hasher::new_derive_key("crab repository push actor v1");
    super::hash_text(&mut digest, issuer);
    super::hash_text(&mut digest, subject);
    *digest.finalize().as_bytes()
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Authority {
    pub repository: Uuid,
    pub ownership_epoch: u32,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Attribution {
    pub publication: RefVersion,
    pub authority: Option<Authority>,
    pub actor: Actor,
    pub predecessors: BTreeMap<String, Option<String>>,
}

#[derive(Deserialize, Serialize)]
struct Transition {
    transaction: String,
    previous: Option<String>,
    authority: Option<Authority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    actor: Option<Actor>,
}

#[derive(Deserialize, Serialize)]
struct Journal {
    epoch: String,
    transitions: Vec<Transition>,
}

fn key(reference: &str) -> String {
    format!(
        "collaboration/v1/push-attribution/{}",
        blake3::hash(reference.as_bytes())
    )
}

pub(crate) struct Record;

impl Mutation for Record {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 477;
    type Input = Attribution;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: Attribution) -> Result<MutationResult<()>> {
        for (reference, previous) in input.predecessors {
            let key = key(&reference);
            let mut journal = context
                .document::<Journal>(&key)?
                .filter(|journal| journal.epoch == input.publication.epoch)
                .unwrap_or_else(|| Journal {
                    epoch: input.publication.epoch.clone(),
                    transitions: Vec::new(),
                });
            // A repeated Cell command cannot rebind this Git attempt to a
            // different predecessor or authorization.
            if journal
                .transitions
                .iter()
                .any(|link| link.transaction == input.publication.transaction)
            {
                continue;
            }
            journal.transitions.push(Transition {
                transaction: input.publication.transaction.clone(),
                previous,
                authority: input.authority,
                actor: Some(input.actor),
            });
            if journal.transitions.len() > MAX_TRANSITIONS {
                let previous = journal
                    .transitions
                    .last()
                    .and_then(|link| link.previous.as_ref());
                let first = &journal.transitions[0].transaction;
                // Failed attempts must not evict the last proven publication.
                journal
                    .transitions
                    .remove(usize::from(previous == Some(first)));
            }
            context.put_document(&key, &journal)?;
        }
        Ok(MutationResult::Success(()))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct LatestInput {
    pub reference: String,
    pub visible: RefVersion,
}

pub(crate) struct Latest;

impl Read for Latest {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 478;
    type Input = LatestInput;
    type Output = Option<Actor>;

    fn execute(context: &mut Transaction<'_>, input: LatestInput) -> Result<Self::Output> {
        Ok(context
            .document::<Journal>(&key(&input.reference))?
            .filter(|journal| journal.epoch == input.visible.epoch)
            .and_then(|journal| {
                journal
                    .transitions
                    .into_iter()
                    .find(|link| link.transaction == input.visible.transaction)
            })
            .and_then(|link| link.actor))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct VerifyInput {
    pub reference: String,
    pub authority: Authority,
    pub previous: RefVersion,
    pub visible: RefVersion,
}

pub(crate) struct Verify;

impl Read for Verify {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 477;
    type Input = VerifyInput;
    type Output = bool;

    fn execute(context: &mut Transaction<'_>, input: VerifyInput) -> Result<bool> {
        let Some(journal) =
            context
                .document::<Journal>(&key(&input.reference))?
                .filter(|journal| {
                    journal.epoch == input.visible.epoch && journal.epoch == input.previous.epoch
                })
        else {
            return Ok(false);
        };
        let links: BTreeMap<_, _> = journal
            .transitions
            .iter()
            .map(|link| (link.transaction.as_str(), link))
            .collect();
        let mut visible = input.visible.transaction.as_str();
        // The live Git head proves the last transaction; each link captured
        // its committed predecessor under the same ref lease as publication.
        for _ in 0..links.len() {
            let Some(link) = links
                .get(visible)
                .filter(|link| link.authority == Some(input.authority))
                .and_then(|link| link.previous.as_deref())
            else {
                return Ok(false);
            };
            visible = link;
            if visible == input.previous.transaction {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
