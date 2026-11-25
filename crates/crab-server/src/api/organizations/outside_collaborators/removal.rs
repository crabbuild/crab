use std::collections::BTreeSet;

use super::*;
use crate::catalog::{
    CatalogDocument, MembershipActor,
    audit::Position,
    outside_removal::{Confirmed, Impact, MAX_COLLABORATORS},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    collaborators: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "OutsideRemovalPlan"))]
pub(in crate::api) struct RemovalPlan {
    organization_revision: u64,
    catalog_revision: u64,
    collaborators: Vec<String>,
    confirmation: String,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "OutsideRemovalPreview"))]
pub(in crate::api) struct RemovalPreview {
    plan: RemovalPlan,
    impact: Impact,
    repository_names: Vec<String>,
}

impl Selection {
    fn validate(collaborators: Vec<String>) -> Result<BTreeSet<String>> {
        let count = collaborators.len();
        if !(1..=MAX_COLLABORATORS).contains(&count)
            || collaborators
                .iter()
                .any(|id| blake3::Hash::from_hex(id).is_err())
        {
            return Err(Error::Invalid(
                "Select between 1 and 50 outside collaborators",
            ));
        }
        let selection: BTreeSet<_> = collaborators.into_iter().collect();
        if selection.len() != count {
            return Err(Error::Invalid("Select each outside collaborator once"));
        }
        Ok(selection)
    }
}

impl RemovalPlan {
    fn sign(&self, server: &Server, organization: Uuid, actor: Uuid) -> blake3::Hash {
        let mut key = blake3::Hasher::new_keyed(&server.cursor_key);
        key.update(b"crab-server.outside-removal-confirmation.v1");
        key.update(organization.as_bytes());
        key.update(actor.as_bytes());
        key.update(&self.organization_revision.to_le_bytes());
        key.update(&self.catalog_revision.to_le_bytes());
        for collaborator in &self.collaborators {
            key.update(collaborator.as_bytes());
        }
        key.finalize()
    }

    fn position(&self, confirmation: blake3::Hash) -> Result<Position> {
        // A confirmation always addresses the same audit object, even after
        // later catalog changes or a restart.
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&confirmation.as_bytes()[..16]);
        let revision = self
            .catalog_revision
            .checked_add(1)
            .ok_or(Error::Invalid("Catalog revision is invalid"))?;
        Ok(Position {
            revision,
            id: Uuid::from_bytes(bytes),
        })
    }
}

struct Targets {
    subjects: BTreeSet<String>,
    impact: Impact,
    repository_names: Vec<String>,
}

fn targets(
    server: &Server,
    snapshot: &CatalogDocument,
    organization: Uuid,
    issuer: &str,
    selected: &BTreeSet<String>,
) -> Result<Targets> {
    let mut subjects = BTreeSet::new();
    let mut repositories = 0;
    let mut grants = 0;
    let mut repository_names = Vec::new();
    for repo in snapshot
        .repositories
        .iter()
        .filter(|repo| repo.owns_direct_grants(organization))
    {
        let mut affected = 0;
        for grant in &repo.members {
            if selected.contains(
                collaborator_key(server, organization, issuer, &grant.subject)
                    .to_hex()
                    .as_str(),
            ) {
                subjects.insert(grant.subject.clone());
                affected += 1;
            }
        }
        if affected > 0 {
            repositories += 1;
            grants += affected;
            if repository_names.len() < 20 {
                repository_names.push(repo.name.clone());
            }
        }
    }
    if subjects.len() != selected.len() {
        return Err(Error::PageChanged);
    }
    Ok(Targets {
        impact: Impact {
            collaborators: subjects.len(),
            repositories,
            grants,
        },
        subjects,
        repository_names,
    })
}

async fn ensure_outside(
    cells: &CellClient,
    address: Address,
    actor: Uuid,
    revision: u64,
    issuer: &str,
    subjects: &BTreeSet<String>,
) -> Result<()> {
    let accounts = subjects
        .iter()
        .map(|subject| IdentityKey {
            issuer: issuer.into(),
            subject: subject.clone(),
        })
        .collect();
    let ids = app::query_output(
        cells
            .query::<LookupAccounts>(Address::Directory, None, accounts)
            .await,
    )?;
    if ids.len() != subjects.len() {
        return Err(Error::CellContract(
            "Outside collaborator lookup returned an invalid result count",
        ));
    }
    let active = cells
        .query::<InspectMemberships>(
            address,
            None,
            MembershipInspection {
                actor,
                revision,
                candidates: ids.into_iter().flatten().collect(),
            },
        )
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::PageChanged)?;
    if !active.is_empty() {
        return Err(Error::PageChanged);
    }
    Ok(())
}

pub(super) async fn preview(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
    Json(input): Json<Selection>,
) -> Result<Json<RemovalPreview>> {
    let selected = Selection::validate(input.collaborators)?;
    let address = target(organization)?;
    let caller = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let revision = owner_revision(&cells, address, caller).await?;
    let issuer = server.auth.as_ref().ok_or(Error::NotFound)?.issuer();
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (snapshot, _) = catalog
        .load_snapshot()
        .await
        .map_err(app::repository_access_error)?;
    let targets = targets(&server, &snapshot, organization, issuer, &selected)?;
    ensure_outside(&cells, address, caller, revision, issuer, &targets.subjects).await?;
    let mut plan = RemovalPlan {
        organization_revision: revision,
        catalog_revision: snapshot.version,
        collaborators: selected.into_iter().collect(),
        confirmation: String::new(),
    };
    plan.confirmation = plan
        .sign(&server, organization, caller)
        .to_hex()
        .to_string();
    Ok(Json(RemovalPreview {
        plan,
        impact: targets.impact,
        repository_names: targets.repository_names,
    }))
}

pub(super) async fn remove(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
    Json(plan): Json<RemovalPlan>,
) -> Result<Json<Impact>> {
    let selected = Selection::validate(plan.collaborators.clone())?;
    let caller = actor(&server, &principal).await?;
    let confirmation = plan.sign(&server, organization, caller);
    if plan.confirmation != confirmation.to_hex().as_str() {
        return Err(Error::Invalid(
            "Review this outside collaborator removal before confirming",
        ));
    }
    let position = plan.position(confirmation)?;
    let identity = app::account(&principal)?;
    // Membership decisions and Git publication share this lease. Resolve the
    // still-external accounts and write the exact catalog revision inside it.
    let result = super::super::properties::policy_write(
        server,
        organization,
        move |server, address, cancel| async move {
            let cells = server.cells().ok_or(Error::CellUnavailable)?;
            let revision = owner_revision(&cells, address, caller).await?;
            let catalog = server.catalog().ok_or(Error::NotFound)?;
            let (snapshot, _) = catalog
                .load_snapshot()
                .await
                .map_err(app::repository_access_error)?;
            if let Some(receipt) = catalog
                .outside_removal_receipt(&snapshot, organization, position, &plan.confirmation)
                .await
                .map_err(app::repository_access_error)?
            {
                return Ok(receipt);
            }
            if plan.catalog_revision != snapshot.version || plan.organization_revision != revision {
                return Err(Error::PageChanged);
            }
            let issuer = server.auth.as_ref().ok_or(Error::NotFound)?.issuer();
            let targets = targets(&server, &snapshot, organization, issuer, &selected)?;
            ensure_outside(&cells, address, caller, revision, issuer, &targets.subjects).await?;
            let impact = catalog
                .remove_outside_grants(
                    Confirmed {
                        position,
                        organization,
                        confirmation: plan.confirmation,
                        actor: MembershipActor {
                            issuer: identity.issuer,
                            subject: identity.subject,
                        },
                        subjects: targets.subjects,
                    },
                    &cancel,
                )
                .await
                .map_err(app::repository_access_error)?;
            Ok(impact)
        },
    )
    .await?;
    Ok(Json(result))
}
