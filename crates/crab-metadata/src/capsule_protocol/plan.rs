use bytes::Bytes;
use crab_storage::{StorageError, Store, StoreLayout};
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::validation::validate_content_hash;

use super::{CapsuleTransaction, CapsuleTransactionRecord, CapsuleTransactionStatus};

const CAPSULE_PLAN_VERSION: u32 = 1;
const MAX_CAPSULE_PLAN_BYTES: u64 = 64 * 1024;

/// Immutable binding between one reviewed plan and its capsule commit marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsulePlanReceipt {
    version: u32,
    repo_prefix: String,
    plan_id: String,
    activation_id: String,
    transaction: CapsuleTransaction,
}

impl CapsulePlanReceipt {
    #[must_use]
    pub fn plan_id(&self) -> &str {
        &self.plan_id
    }

    #[must_use]
    pub fn activation_id(&self) -> &str {
        &self.activation_id
    }

    #[must_use]
    pub fn transaction(&self) -> &CapsuleTransaction {
        &self.transaction
    }
}

/// Require a plan with no durable capsule publication attempt.
pub async fn ensure_capsule_plan_unattempted(
    store: &Store,
    router: &StoreLayout<Store>,
    plan_id: &str,
) -> Result<()> {
    validate_content_hash(plan_id, "plan id", "capsule publication admission")?;
    let intent = read_optional(store, &router.capsule_plan_intent_path(plan_id)).await?;
    let receipt = read_optional(store, &router.capsule_plan_receipt_path(plan_id)).await?;
    for record in intent.iter().chain(receipt.iter()) {
        validate(router, record)?;
        if record.plan_id() != plan_id {
            return Err(corrupt(
                "capsule publication admission",
                "plan object key does not match its plan identity",
            ));
        }
    }
    if intent.is_some() || receipt.is_some() {
        return Err(MetadataError::PlanAlreadyAttempted {
            plan_id: plan_id.to_owned(),
        });
    }
    Ok(())
}

/// Persist the plan binding before any ref head can become visible.
pub async fn prepare_capsule_plan(
    store: &Store,
    router: &StoreLayout<Store>,
    transaction: &CapsuleTransaction,
    activation_id: &str,
) -> Result<CapsulePlanReceipt> {
    let plan_id = transaction.plan_id().ok_or_else(|| {
        corrupt(
            "capsule mirror plan intent",
            "capsule transaction has no mirror plan identity",
        )
    })?;
    let intent = CapsulePlanReceipt {
        version: CAPSULE_PLAN_VERSION,
        repo_prefix: router.repo_prefix().to_owned(),
        plan_id: plan_id.to_owned(),
        activation_id: activation_id.to_owned(),
        transaction: transaction.clone(),
    };
    validate(router, &intent)?;
    write_exact(store, &router.capsule_plan_intent_path(plan_id), &intent).await?;
    Ok(intent)
}

/// Publish a terminal receipt after the exact activation marker is durable.
pub async fn publish_capsule_plan_receipt(
    store: &Store,
    router: &StoreLayout<Store>,
    intent: &CapsulePlanReceipt,
) -> Result<CapsulePlanReceipt> {
    validate(router, intent)?;
    validate_commit(store, router, intent).await?;
    write_exact(
        store,
        &router.capsule_plan_receipt_path(intent.plan_id()),
        intent,
    )
    .await?;
    Ok(intent.clone())
}

/// Resolve historical commitment and repair a missing terminal receipt.
pub async fn resolve_capsule_plan_receipt(
    store: &Store,
    router: &StoreLayout<Store>,
    plan_id: &str,
) -> Result<Option<CapsulePlanReceipt>> {
    validate_content_hash(plan_id, "plan id", "capsule mirror plan receipt")?;
    if let Some(receipt) = read_optional(store, &router.capsule_plan_receipt_path(plan_id)).await? {
        validate(router, &receipt)?;
        if receipt.plan_id() != plan_id {
            return Err(corrupt(
                "capsule mirror plan receipt",
                "terminal key does not match its plan identity",
            ));
        }
        validate_commit(store, router, &receipt).await?;
        return Ok(Some(receipt));
    }
    let Some(intent) = read_optional(store, &router.capsule_plan_intent_path(plan_id)).await?
    else {
        return Ok(None);
    };
    validate(router, &intent)?;
    if intent.plan_id() != plan_id || !commit_is_visible(store, router, &intent).await? {
        return Ok(None);
    }
    publish_capsule_plan_receipt(store, router, &intent)
        .await
        .map(Some)
}

async fn validate_commit(
    store: &Store,
    router: &StoreLayout<Store>,
    receipt: &CapsulePlanReceipt,
) -> Result<()> {
    if commit_is_visible(store, router, receipt).await? {
        return Ok(());
    }
    Err(corrupt(
        router.capsule_plan_receipt_path(receipt.plan_id()).as_ref(),
        "capsule mirror plan receipt names an unavailable commit marker",
    ))
}

async fn commit_is_visible(
    store: &Store,
    router: &StoreLayout<Store>,
    receipt: &CapsulePlanReceipt,
) -> Result<bool> {
    let path = router.capsule_transaction_path(receipt.activation_id());
    let (body, _) = match store
        .get_with_etag_bounded(&path, super::MAX_CAPSULE_TRANSACTION_RECORD_BYTES)
        .await
    {
        Ok(value) => value,
        Err(StorageError::NotFound { .. }) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let record = CapsuleTransactionRecord::decode(&body)?;
    if record.activation_id() != receipt.activation_id()
        || record.transaction_id() != receipt.transaction.id()?
    {
        return Err(corrupt(
            path.as_ref(),
            "transaction record does not match its plan intent",
        ));
    }
    if record.status() != CapsuleTransactionStatus::Committed {
        return Ok(false);
    }
    ensure_commit_marker(store, router, &record, body).await?;
    Ok(true)
}

async fn ensure_commit_marker(
    store: &Store,
    router: &StoreLayout<Store>,
    record: &CapsuleTransactionRecord,
    body: Bytes,
) -> Result<()> {
    let path = router.capsule_committed_transaction_path(record.activation_id());
    if store.put_if_absent_verified(&path, body.clone()).await? {
        return Ok(());
    }
    let (actual, _) = store
        .get_with_etag_bounded(&path, super::MAX_CAPSULE_TRANSACTION_RECORD_BYTES)
        .await?;
    if actual == body {
        Ok(())
    } else {
        Err(corrupt(
            path.as_ref(),
            "committed marker conflicts with its transaction record",
        ))
    }
}

fn validate(router: &StoreLayout<Store>, receipt: &CapsulePlanReceipt) -> Result<()> {
    if receipt.version != CAPSULE_PLAN_VERSION || receipt.repo_prefix != router.repo_prefix() {
        return Err(corrupt(
            "capsule mirror plan receipt",
            "receipt version or repository identity is invalid",
        ));
    }
    validate_content_hash(
        &receipt.plan_id,
        "mirror plan id",
        "capsule mirror plan receipt",
    )?;
    validate_content_hash(
        &receipt.activation_id,
        "activation id",
        "capsule mirror plan receipt",
    )?;
    if receipt.transaction.plan_id() != Some(receipt.plan_id.as_str()) {
        return Err(corrupt(
            "capsule mirror plan receipt",
            "transaction does not commit the receipt plan identity",
        ));
    }
    receipt.transaction.id().map(|_| ())
}

async fn read_optional(
    store: &Store,
    path: &object_store::path::Path,
) -> Result<Option<CapsulePlanReceipt>> {
    let (body, _) = match store
        .get_with_etag_bounded(path, MAX_CAPSULE_PLAN_BYTES)
        .await
    {
        Ok(value) => value,
        Err(StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let receipt: CapsulePlanReceipt = serde_json::from_slice(&body)
        .map_err(|error| corrupt(path.as_ref(), format!("invalid plan receipt JSON: {error}")))?;
    let canonical = serde_json::to_vec(&receipt)
        .map_err(|error| MetadataError::Internal(format!("serialize capsule plan: {error}")))?;
    if canonical != body {
        return Err(corrupt(
            path.as_ref(),
            "plan object is not canonically encoded",
        ));
    }
    Ok(Some(receipt))
}

async fn write_exact(
    store: &Store,
    path: &object_store::path::Path,
    receipt: &CapsulePlanReceipt,
) -> Result<()> {
    let body = serde_json::to_vec(receipt)
        .map(Bytes::from)
        .map_err(|error| MetadataError::Internal(format!("serialize capsule plan: {error}")))?;
    match store.create_strict(path, body.clone()).await {
        Ok(()) => Ok(()),
        Err(StorageError::StateConflict { .. }) => {
            let (actual, _) = store
                .get_with_etag_bounded(path, MAX_CAPSULE_PLAN_BYTES)
                .await?;
            if actual == body {
                Ok(())
            } else {
                Err(corrupt(
                    path.as_ref(),
                    "plan object conflicts with its immutable identity",
                ))
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn corrupt(path: &str, reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: path.to_owned(),
        reason: reason.into(),
    }
}
