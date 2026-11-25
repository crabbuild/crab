use std::collections::BTreeSet;

use super::*;
use crate::catalog::{
    MembershipActor,
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

async fn targets(snapshot: &Snapshot<'_>, selected: &BTreeSet<String>) -> Result<Targets> {
    let subjects: Vec<_> = snapshot
        .grants()
        .into_keys()
        .filter(|subject| selected.contains(&snapshot.collaborator_key(subject)))
        .collect();
    if subjects.len() != selected.len()
        || snapshot.outsiders(&subjects).await?.len() != selected.len()
    {
        return Err(Error::PageChanged);
    }
    let subjects: BTreeSet<_> = subjects.into_iter().map(str::to_owned).collect();
    let mut impact = Impact {
        collaborators: subjects.len(),
        repositories: 0,
        grants: 0,
    };
    let mut repository_names = Vec::new();
    for repo in snapshot.document.organization_grant_records(snapshot.id) {
        let count = repo
            .members
            .iter()
            .filter(|grant| subjects.contains(&grant.subject))
            .count();
        if count > 0 {
            impact.repositories += 1;
            impact.grants += count;
            if repository_names.len() < 20 {
                repository_names.push(repo.name.clone());
            }
        }
    }
    Ok(Targets {
        subjects,
        impact,
        repository_names,
    })
}

pub(super) async fn preview(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
    Json(input): Json<Selection>,
) -> Result<Json<RemovalPreview>> {
    let selected = Selection::validate(input.collaborators)?;
    let snapshot = Snapshot::load(&server, &principal, organization).await?;
    let targets = targets(&snapshot, &selected).await?;
    snapshot.verify().await?;
    let mut plan = RemovalPlan {
        organization_revision: snapshot.version,
        catalog_revision: snapshot.document.version,
        collaborators: selected.into_iter().collect(),
        confirmation: String::new(),
    };
    plan.confirmation = plan
        .sign(&server, organization, snapshot.actor)
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
    owner_version(&server, organization, caller).await?;
    let worker = Arc::clone(&server);
    let result = with_org_policy(&server, organization, move |cancel| async move {
        let snapshot = Snapshot::load(&worker, &principal, organization).await?;
        let catalog = worker.catalog().ok_or(Error::NotFound)?;
        if let Some(receipt) = catalog
            .outside_removal_receipt(
                &snapshot.document,
                organization,
                position,
                &plan.confirmation,
            )
            .await
            .map_err(app::repository_access_error)?
        {
            app::account(&principal)?;
            return Ok(receipt);
        }
        if plan.catalog_revision != snapshot.document.version
            || plan.organization_revision != snapshot.version
        {
            return Err(Error::PageChanged);
        }
        let targets = targets(&snapshot, &selected).await?;
        let identity = app::account(&principal)?;
        catalog
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
            .map_err(app::repository_access_error)
    })
    .await?;
    Ok(Json(result))
}
