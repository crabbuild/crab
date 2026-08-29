//! Maintained inventory of Crab-owned canonical-v1 contracts.

#![allow(clippy::unwrap_used, reason = "test assertions")]

use crab_metadata::layout_descriptor::{LAYOUT_DESCRIPTOR_SCHEMA_VERSION, LayoutDescriptor};
use crab_metadata::manifests::{MANIFEST_VERSION, Manifest, validate_manifest_payload};
use crab_metadata::receipts::RECEIPT_SCHEMA_VERSION;
use crab_metadata::ref_registry::{
    REF_REGISTRY_RECORD_SCHEMA_VERSION, REF_REGISTRY_SCHEMA_VERSION,
};
use crab_metadata::segmented::SEGMENT_INDEX_VERSION;
use crab_staging::push_plan::FILE_PUSH_PLAN_VERSION;
use crab_types::pointer::VERSION_LINE;
use crab_workflow::{LOCKFILE_SCHEMA_VERSION, artifact::ARTIFACT_SCHEMA_VERSION};

#[test]
fn crab_owned_contract_inventory_is_canonical_v1() {
    assert_eq!(LAYOUT_DESCRIPTOR_SCHEMA_VERSION, 1);
    assert_eq!(MANIFEST_VERSION, 1);
    assert_eq!(RECEIPT_SCHEMA_VERSION, 1);
    assert_eq!(REF_REGISTRY_SCHEMA_VERSION, 1);
    assert_eq!(REF_REGISTRY_RECORD_SCHEMA_VERSION, 1);
    assert_eq!(SEGMENT_INDEX_VERSION, 1);
    assert_eq!(FILE_PUSH_PLAN_VERSION, 1);
    assert_eq!(LOCKFILE_SCHEMA_VERSION, 1);
    assert_eq!(ARTIFACT_SCHEMA_VERSION, 1);
    assert_eq!(VERSION_LINE, "version https://crab.dev/spec/v1");
}

#[test]
fn non_v1_remote_contracts_fail_closed() {
    let canonical = LayoutDescriptor::canonical();
    let mut descriptor = serde_json::to_value(canonical).unwrap();
    descriptor["schema_version"] = serde_json::json!(2);
    assert!(
        LayoutDescriptor::parse("org/repo/layout", &serde_json::to_vec(&descriptor).unwrap())
            .is_err()
    );

    let mut manifest = Manifest::default_for_repo("refs/heads/main");
    manifest.version = 2;
    assert!(validate_manifest_payload(&manifest).is_err());
}
