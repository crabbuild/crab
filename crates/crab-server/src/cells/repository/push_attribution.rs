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

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Authority {
    pub repository: Uuid,
    pub ownership_epoch: u32,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Attribution {
    pub publication: RefVersion,
    pub authority: Authority,
    pub predecessors: BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize)]
struct Transition {
    transaction: String,
    previous: String,
    authority: Authority,
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
            });
            let prune = journal.transitions.len().saturating_sub(MAX_TRANSITIONS);
            journal.transitions.drain(..prune);
            context.put_document(&key, &journal)?;
        }
        Ok(MutationResult::Success(()))
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
                .filter(|link| link.authority == input.authority)
            else {
                return Ok(false);
            };
            visible = &link.previous;
            if visible == input.previous.transaction {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
