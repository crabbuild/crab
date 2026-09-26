//! Cluster qualification receipt acceptance and rejection.
//!
//! `validate_cluster_receipt` gates the four-process failover receipt before a
//! protected provider workflow accepts it as release evidence, so the envelope
//! and every evidence clause need a case that proves the receipt is rejected
//! when that clause does not hold.

use crab_cell_runtime::identity::Digest;
use crab_cell_runtime::qualification::cluster::validate_cluster_receipt;
use serde_json::{Value, json};

const SOURCE_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
const IMAGE: Digest = Digest::from_bytes([0xaa; 32]);
const PROJECT: &str = "crab-http-cluster-qualification-receipt-contract";
const MAX_RECEIPT_BYTES: usize = 8 << 20;

fn repeated(byte: u8, bytes: usize) -> String {
    format!("{byte:02x}").repeat(bytes)
}

fn session(byte: u8) -> String {
    repeated(byte, 16)
}

fn node(byte: u8) -> String {
    repeated(byte, 16)
}

fn image_digest() -> String {
    format!("sha256:{}", repeated(0xaa, 32))
}

fn root(byte: u8, checksum: u64, txid: u64, commit_sequence: u64) -> Value {
    json!({
        "digest": repeated(byte, 32),
        "checksum": checksum,
        "txid": txid,
        "commit_sequence": commit_sequence,
    })
}

fn timing() -> Value {
    json!({
        "owner_killed_ms": 10,
        "advertisement_expired_ms": 20,
        "recovery_sealed_ms": 30,
        "first_served_ms": 40,
    })
}

fn work_cycle() -> Value {
    json!({
        "candidate_count": 1,
        "affected_cells": 1,
        "catalog_shards": 1,
        "catalog_pages": 1,
        "control_reads": 1,
        "follower_pages": 1,
        "follower_frames": 1,
        "follower_bytes": 1,
        "peer_requests": 1,
        "bundle_bytes": 1,
        "object_reads": 1,
        "object_writes": 1,
        "phases": {
            "claim": {"count": 1, "duration_ms": 1},
            "witness": {"count": 1, "duration_ms": 1},
            "scope_validation": {"count": 1, "duration_ms": 1},
            "pin_attach": {"count": 1, "duration_ms": 1},
            "seal": {"count": 1, "duration_ms": 1},
        },
    })
}

fn empty_work_cycle() -> Value {
    let mut cycle = work_cycle();
    for field in [
        "candidate_count",
        "affected_cells",
        "catalog_shards",
        "catalog_pages",
        "control_reads",
        "follower_pages",
        "follower_frames",
        "follower_bytes",
        "peer_requests",
        "bundle_bytes",
        "object_reads",
        "object_writes",
    ] {
        cycle[field] = json!(0);
    }
    for phase in ["claim", "witness", "scope_validation", "pin_attach", "seal"] {
        cycle["phases"][phase] = json!({"count": 0, "duration_ms": 0});
    }
    cycle
}

fn selection_cycle(
    failed_session: &str,
    successor_session: &str,
    failed_node: &str,
    successor_node: &str,
    members: &[String],
    original_follower: bool,
) -> Value {
    json!({
        "failed_session": failed_session,
        "successor_session": successor_session,
        "failed_node": failed_node,
        "successor_node": successor_node,
        "failed_log_members": members,
        "selected_original_follower": original_follower,
        "terminal_result": "succeeded",
    })
}

/// One internally consistent four-process failover receipt.
fn canonical_receipt() -> Value {
    let failed_session = session(0x11);
    let successor_session = session(0x22);
    let second_successor_session = session(0x33);
    let fallback_successor_session = session(0x44);
    let failed_node = node(0xa1);
    let successor_node = node(0xb2);
    let second_successor_node = node(0xc3);
    let fallback_successor_node = node(0xd4);
    let first_root = root(0x11, 10, 1, 1);
    let restored_root = root(0x22, 20, 2, 2);
    let continued_root = root(0x33, 30, 3, 3);
    let second_root = root(0x44, 40, 4, 4);
    let fallback_root = root(0x55, 50, 5, 5);
    let first_members = vec![
        failed_node.clone(),
        successor_node.clone(),
        second_successor_node.clone(),
    ];
    let node_capacity = || {
        json!({
            "version": 1,
            "resources": {"memory_bytes": 1_000, "free_disk_bytes": 1_000},
            "admission": {"active_cells": 1},
        })
    };
    let placement_node = |session: &str, node: &str| {
        json!({
            "live": true,
            "session": session,
            "advertisement": {"node": node, "placement": {}, "log": {}},
        })
    };
    json!({
        "version": 6,
        "source_revision": SOURCE_REVISION,
        "image": {"reference": "source-only", "digest": image_digest()},
        "project": PROJECT,
        "owner_loss": {
            "session_before": failed_session,
            "session_after": successor_session,
            "epoch_before": 1,
            "epoch_after": 2,
            "timing": timing(),
            "root_before": first_root,
            "root_after_restore": restored_root,
            "root_continued": continued_root,
        },
        "fleet_only_commit": {
            "owner_uncovered_bytes": 1,
            "follower_retained_bytes": 1,
            "immutable_object_put_rejected": true,
            "owner_disk_removed_before_policy_restore": true,
            "control_before_owner_loss": {
                "state": "serving",
                "epoch": 1,
                "owner": {"session": failed_session},
                "owner_lease": {"state": "live", "observed_at_ms": 5, "expires_at_ms": 10},
                "root": first_root,
            },
            "node_log_before": {"member_nodes": first_members},
            "node_log_after": {"state": "open", "epoch": 2, "active": true, "member_nodes": first_members},
            "response": {"id": 1},
            "restored_labels": {"items": ["owner-loss"]},
        },
        "follower_replacement": {
            "node_log_before": {"epoch": 1, "member_nodes": first_members},
            "node_log_after": {"epoch": 2, "member_nodes": [second_successor_node]},
            "object_covered_response": {"number": 3},
            "fleet_only_response": {"id": 2},
        },
        "second_owner_loss": {
            "session_before": successor_session,
            "session_after": second_successor_session,
            "epoch_before": 2,
            "epoch_after": 3,
            "timing": timing(),
            "root_before": continued_root,
            "root_after": second_root,
            "restored_labels": {"items": ["first", "second"]},
        },
        "fallback_owner_loss": {
            "session_before": second_successor_session,
            "session_after": fallback_successor_session,
            "epoch_before": 3,
            "epoch_after": 4,
            "timing": timing(),
            "root_before": second_root,
            "root_after": fallback_root,
            "node_log_before": {"active": true, "member_nodes": first_members},
            "response": {"id": 3},
            "restored_labels": {"items": ["first", "second", "third"]},
            "candidate_record": {
                "live": true,
                "session": fallback_successor_session,
                "advertisement": {"node": fallback_successor_node},
            },
        },
        "capacity": {
            "node_a": node_capacity(),
            "node_b": node_capacity(),
            "node_c": node_capacity(),
            "node_d": node_capacity(),
        },
        "measured_disk": {
            "node_a_bytes": 1_000,
            "node_b_bytes": 1_000,
            "node_c_bytes": 1_000,
            "node_d_bytes": 1_000,
            "tolerance_bytes": 1,
        },
        "placement": {
            "node_a": placement_node(&failed_session, &failed_node),
            "node_b": placement_node(&successor_session, &successor_node),
            "node_c": placement_node(&second_successor_session, &second_successor_node),
            "node_d": placement_node(&fallback_successor_session, &fallback_successor_node),
            "node_a_session": failed_session,
            "node_b_session": successor_session,
            "node_c_session": second_successor_session,
            "node_d_session": fallback_successor_session,
        },
        "metrics": {
            "node_a": "crab_cell_durability_proofs_total 1",
            "node_b": "crab_cell_durability_proofs_total 1",
            "node_c": "crab_cell_durability_proofs_total 1",
            "node_d": "crab_cell_durability_proofs_total 1",
        },
        "capacity_metric_parity": {
            "local_disk": true,
            "active_cells": true,
            "measured_local_disk": true,
            "signed_placement": true,
        },
        "selection": {
            "owner_loss": selection_cycle(
                &failed_session,
                &successor_session,
                &failed_node,
                &successor_node,
                &first_members,
                true,
            ),
            "second_owner_loss": selection_cycle(
                &successor_session,
                &second_successor_session,
                &successor_node,
                &second_successor_node,
                std::slice::from_ref(&second_successor_node),
                true,
            ),
            "fallback": selection_cycle(
                &second_successor_session,
                &fallback_successor_session,
                &second_successor_node,
                &fallback_successor_node,
                &first_members,
                false,
            ),
        },
        "work": {
            "owner_loss": work_cycle(),
            "second_owner_loss": work_cycle(),
            "fallback": work_cycle(),
        },
    })
}

fn expect_rejected(receipt: &Value, published_image: bool, expected: &str) {
    let bytes = serde_json::to_vec(receipt).unwrap();
    let error = validate_cluster_receipt(&bytes, SOURCE_REVISION, IMAGE, published_image)
        .err()
        .unwrap_or_else(|| panic!("receipt was accepted but must be rejected: {expected}"));
    assert!(
        error.to_string().contains(expected),
        "expected an error containing {expected:?}, got {error}"
    );
}

#[test]
fn canonical_failover_receipt_is_accepted() {
    let bytes = serde_json::to_vec(&canonical_receipt()).unwrap();
    assert!(validate_cluster_receipt(&bytes, SOURCE_REVISION, IMAGE, false).is_ok());

    let mut published = canonical_receipt();
    published["image"]["reference"] = json!(format!(
        "ghcr.io/crabbuild/crab-http-server@{}",
        image_digest()
    ));
    let bytes = serde_json::to_vec(&published).unwrap();
    assert!(validate_cluster_receipt(&bytes, SOURCE_REVISION, IMAGE, true).is_ok());
}

#[test]
fn follower_selection_uses_the_log_that_protected_the_acknowledged_commit() {
    let mut receipt = canonical_receipt();
    // A fully covered log can be replaced before the follower-only write.
    // Its old members must not override the active log recorded after that write.
    receipt["fleet_only_commit"]["node_log_before"] =
        json!({"state": "open", "epoch": 1, "active": false, "member_nodes": [node(0xc3)]});
    let bytes = serde_json::to_vec(&receipt).unwrap();
    assert!(validate_cluster_receipt(&bytes, SOURCE_REVISION, IMAGE, false).is_ok());
}

#[test]
fn follower_selection_requires_an_active_acknowledging_log() {
    for (field, value) in [
        ("state", json!("closed")),
        ("epoch", json!(0)),
        ("active", json!(false)),
        ("member_nodes", json!([])),
    ] {
        let mut receipt = canonical_receipt();
        receipt["fleet_only_commit"]["node_log_after"][field] = value;
        expect_rejected(&receipt, false, "fleet-only active log");
    }
}

#[test]
fn receipt_envelope_rejects_malformed_and_mismatched_bytes() {
    assert!(validate_cluster_receipt(&[], SOURCE_REVISION, IMAGE, false).is_err());
    assert!(
        validate_cluster_receipt(
            &vec![b'{'; MAX_RECEIPT_BYTES + 1],
            SOURCE_REVISION,
            IMAGE,
            false
        )
        .is_err()
    );
    assert!(validate_cluster_receipt(b"{", SOURCE_REVISION, IMAGE, false).is_err());

    let mut version = canonical_receipt();
    version["version"] = json!(5);
    expect_rejected(&version, false, "receipt version");

    let mut revision = canonical_receipt();
    revision["source_revision"] = json!(repeated(0x01, 20));
    expect_rejected(&revision, false, "source revision");

    let mut digest = canonical_receipt();
    digest["image"]["digest"] = json!(format!("sha256:{}", repeated(0xbb, 32)));
    expect_rejected(&digest, false, "image digest");

    let mut published_under_source_mode = canonical_receipt();
    published_under_source_mode["image"]["reference"] = json!(format!(
        "ghcr.io/crabbuild/crab-http-server@{}",
        image_digest()
    ));
    expect_rejected(&published_under_source_mode, false, "source image");

    let source_under_published_mode = canonical_receipt();
    expect_rejected(&source_under_published_mode, true, "image reference");

    let mut project = canonical_receipt();
    project["project"] = json!("cluster-qualification");
    expect_rejected(&project, false, "project");

    let mut unknown_field = canonical_receipt();
    unknown_field["unexpected_evidence"] = json!(true);
    let bytes = serde_json::to_vec(&unknown_field).unwrap();
    assert!(validate_cluster_receipt(&bytes, SOURCE_REVISION, IMAGE, false).is_err());
}

#[test]
fn receipt_evidence_clauses_reject_tampering() {
    type Mutation = fn(&mut Value);
    let cases: &[(&str, &str, Mutation)] = &[
        (
            "owner loss without a session change",
            "did not change session",
            |receipt| {
                receipt["owner_loss"]["session_after"] = json!(session(0x11));
            },
        ),
        (
            "owner loss without an epoch advance",
            "epoch did not advance",
            |receipt| {
                receipt["owner_loss"]["epoch_after"] = json!(1);
            },
        ),
        (
            "owner loss where restore did not advance the root",
            "did not advance",
            |receipt| {
                receipt["owner_loss"]["root_after_restore"] = root(0x11, 10, 1, 1);
            },
        ),
        (
            "two owner losses from unrelated histories",
            "owner chain",
            |receipt| {
                receipt["owner_loss"]["session_after"] = json!(session(0x99));
            },
        ),
        (
            "fleet-only commit bound to another owner session",
            "owner chain",
            |receipt| {
                receipt["fleet_only_commit"]["control_before_owner_loss"]["owner"]["session"] =
                    json!(session(0xee));
            },
        ),
        (
            "fleet-only commit bound to another root",
            "root chain",
            |receipt| {
                receipt["fleet_only_commit"]["control_before_owner_loss"]["root"]["digest"] =
                    json!(repeated(0xee, 32));
            },
        ),
        (
            "second owner loss that regresses the root",
            "root chain",
            |receipt| {
                receipt["second_owner_loss"]["root_before"] = root(0x22, 20, 2, 2);
            },
        ),
        (
            "owner-loss timing that expires before the kill",
            "timing",
            |receipt| {
                receipt["owner_loss"]["timing"]["owner_killed_ms"] = json!(50);
            },
        ),
        (
            "fleet-only commit without uncovered bytes",
            "fleet-only durability evidence",
            |receipt| {
                receipt["fleet_only_commit"]["owner_uncovered_bytes"] = json!(0);
            },
        ),
        (
            "fleet-only commit with an expired owner lease",
            "fleet-only owner lease",
            |receipt| {
                receipt["fleet_only_commit"]["control_before_owner_loss"]["owner_lease"]["state"] =
                    json!("expired");
            },
        ),
        (
            "fleet-only response that is not the first command",
            "fleet-only response evidence",
            |receipt| {
                receipt["fleet_only_commit"]["response"]["id"] = json!(2);
            },
        ),
        (
            "follower replacement that does not advance the log",
            "follower replacement log",
            |receipt| {
                receipt["follower_replacement"]["node_log_after"]["epoch"] = json!(1);
            },
        ),
        (
            "follower replacement with the wrong object response",
            "follower replacement response",
            |receipt| {
                receipt["follower_replacement"]["object_covered_response"]["number"] = json!(2);
            },
        ),
        (
            "second owner loss with too few restored labels",
            "second owner loss labels",
            |receipt| {
                receipt["second_owner_loss"]["restored_labels"]["items"] = json!(["first"]);
            },
        ),
        (
            "fallback response that is not the third command",
            "fallback owner-loss evidence",
            |receipt| {
                receipt["fallback_owner_loss"]["response"]["id"] = json!(2);
            },
        ),
        (
            "capacity report from another schema",
            "capacity schema",
            |receipt| {
                receipt["capacity"]["node_c"]["version"] = json!(2);
            },
        ),
        (
            "capacity report without usable memory",
            "capacity resources",
            |receipt| {
                receipt["capacity"]["node_b"]["resources"]["memory_bytes"] = json!(0);
            },
        ),
        (
            "measured disk without one node",
            "cluster receipt field",
            |receipt| {
                receipt["measured_disk"]
                    .as_object_mut()
                    .unwrap()
                    .remove("node_d_bytes");
            },
        ),
        (
            "placement with a node that is not live",
            "placement node is not live",
            |receipt| {
                receipt["placement"]["node_d"]["live"] = json!(false);
            },
        ),
        (
            "placement without a signed log",
            "cluster receipt field",
            |receipt| {
                receipt["placement"]["node_a"]["advertisement"]
                    .as_object_mut()
                    .unwrap()
                    .remove("log");
            },
        ),
        (
            "metrics that leak a node identifier",
            "metrics payload contains an identifier",
            |receipt| {
                receipt["metrics"]["node_b"] =
                    json!("crab_cell_node_log_bytes_total{node=\"b\"} 1");
            },
        ),
        (
            "metrics payload that is empty",
            "metrics payload",
            |receipt| {
                receipt["metrics"]["node_c"] = json!("");
            },
        ),
        (
            "capacity metric parity that is not proven",
            "capacity metric parity",
            |receipt| {
                receipt["capacity_metric_parity"]["local_disk"] = json!(false);
            },
        ),
        (
            "successor that was not an original follower",
            "successor was not an original follower",
            |receipt| {
                let members = vec![node(0xa1), node(0xc3)];
                receipt["fleet_only_commit"]["node_log_after"]["member_nodes"] = json!(members);
            },
        ),
        (
            "owner-loss selection that names another node",
            "owner-loss selection identity",
            |receipt| {
                receipt["selection"]["owner_loss"]["failed_node"] = json!(node(0xe5));
            },
        ),
        (
            "owner-loss selection that drops the successor",
            "owner-loss successor is not a follower",
            |receipt| {
                receipt["selection"]["owner_loss"]["failed_log_members"] = json!([node(0xa1)]);
            },
        ),
        (
            "second owner-loss selection outside the replaced log",
            "second owner-loss selection identity",
            |receipt| {
                receipt["selection"]["second_owner_loss"]["successor_node"] = json!(node(0xd4));
            },
        ),
        (
            "fallback selection that claims an original follower",
            "cluster selection evidence",
            |receipt| {
                receipt["selection"]["fallback"]["selected_original_follower"] = json!(true);
            },
        ),
        (
            "recovery work without candidates",
            "cluster recovery work counter",
            |receipt| {
                receipt["work"]["owner_loss"]["candidate_count"] = json!(0);
            },
        ),
        (
            "recovery work without a seal phase",
            "cluster recovery phase evidence",
            |receipt| {
                receipt["work"]["owner_loss"]["phases"]["seal"]["count"] = json!(0);
            },
        ),
        (
            "fallback recovery work that is empty",
            "cluster recovery work counter",
            |receipt| {
                receipt["work"]["fallback"] = empty_work_cycle();
            },
        ),
    ];
    for (name, expected, mutate) in cases {
        let mut receipt = canonical_receipt();
        mutate(&mut receipt);
        let bytes = serde_json::to_vec(&receipt).unwrap();
        let error = validate_cluster_receipt(&bytes, SOURCE_REVISION, IMAGE, false)
            .err()
            .unwrap_or_else(|| panic!("{name}: receipt was accepted"));
        assert!(
            error.to_string().contains(expected),
            "{name}: expected an error containing {expected:?}, got {error}"
        );
    }
}
