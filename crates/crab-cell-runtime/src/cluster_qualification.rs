use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{Digest, Error, Result, identity::encode_hex};

const MAX_CLUSTER_RECEIPT_BYTES: usize = 8 << 20;
const CLUSTER_RECEIPT_SCHEMA_VERSION: u64 = 6;
const MAX_CLUSTER_WORK_VALUE: u64 = 1 << 40;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClusterQualificationReceipt {
    version: u64,
    source_revision: String,
    image: ClusterImageBinding,
    project: String,
    owner_loss: Value,
    fleet_only_commit: Value,
    follower_replacement: Value,
    second_owner_loss: Value,
    fallback_owner_loss: Value,
    capacity: Value,
    measured_disk: Value,
    placement: Value,
    metrics: Value,
    capacity_metric_parity: Value,
    selection: ClusterSelectionEvidence,
    work: ClusterWorkEvidence,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClusterImageBinding {
    reference: String,
    digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClusterSelectionEvidence {
    owner_loss: SelectionCycle,
    second_owner_loss: SelectionCycle,
    fallback: SelectionCycle,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionCycle {
    failed_session: String,
    successor_session: String,
    failed_node: String,
    successor_node: String,
    failed_log_members: Vec<String>,
    selected_original_follower: bool,
    terminal_result: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClusterWorkEvidence {
    owner_loss: WorkCycle,
    second_owner_loss: WorkCycle,
    fallback: WorkCycle,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkCycle {
    candidate_count: u64,
    affected_cells: u64,
    catalog_shards: u64,
    catalog_pages: u64,
    control_reads: u64,
    follower_pages: u64,
    follower_frames: u64,
    follower_bytes: u64,
    peer_requests: u64,
    bundle_bytes: u64,
    object_reads: u64,
    object_writes: u64,
    phases: RecoveryPhaseEvidence,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryPhaseEvidence {
    claim: RecoveryPhaseTiming,
    witness: RecoveryPhaseTiming,
    scope_validation: RecoveryPhaseTiming,
    pin_attach: RecoveryPhaseTiming,
    seal: RecoveryPhaseTiming,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryPhaseTiming {
    count: u64,
    duration_ms: u64,
}

/// Validates one raw four-process failover receipt against its release identity.
///
/// The receipt is intentionally parsed as a strict top-level record while
/// opaque API responses and Prometheus text remain values. This keeps the
/// evidence contract narrow without duplicating every product response type.
pub fn validate_cluster_receipt(
    bytes: &[u8],
    source_revision: &str,
    image: Digest,
    require_published_image: bool,
) -> Result<()> {
    if bytes.is_empty() || bytes.len() > MAX_CLUSTER_RECEIPT_BYTES {
        return Err(Error::Control("cluster qualification receipt size"));
    }
    let receipt: ClusterQualificationReceipt = serde_json::from_slice(bytes)?;
    if receipt.version != CLUSTER_RECEIPT_SCHEMA_VERSION {
        return Err(Error::Control("cluster qualification receipt version"));
    }
    validate_revision(source_revision)?;
    if receipt.source_revision != source_revision {
        return Err(Error::Control("cluster qualification source revision"));
    }
    let expected_digest = format!("sha256:{}", encode_hex(image.as_bytes()));
    if receipt.image.digest != expected_digest {
        return Err(Error::Control("cluster qualification image digest"));
    }
    if require_published_image {
        if !receipt.image.reference.starts_with("ghcr.io/")
            || !receipt
                .image
                .reference
                .ends_with(&format!("@{expected_digest}"))
            || receipt.image.reference.len() <= "ghcr.io/@".len() + expected_digest.len()
        {
            return Err(Error::Control("cluster qualification image reference"));
        }
    } else if receipt.image.reference != "source-only" {
        return Err(Error::Control("cluster qualification source image"));
    }
    let project_prefix = "crab-http-cluster-qualification-";
    if !receipt.project.starts_with(project_prefix) || receipt.project.len() == project_prefix.len()
    {
        return Err(Error::Control("cluster qualification project"));
    }

    validate_owner_loss(&receipt.owner_loss)?;
    validate_fleet_only_commit(&receipt.fleet_only_commit)?;
    validate_follower_replacement(&receipt.follower_replacement)?;
    validate_second_owner_loss(&receipt.second_owner_loss)?;
    validate_fallback_owner_loss(&receipt.fallback_owner_loss)?;
    validate_capacity(&receipt.capacity)?;
    validate_measured_disk(&receipt.measured_disk)?;
    validate_placement(&receipt.placement)?;
    validate_follower_affinity(
        &receipt.owner_loss,
        &receipt.fleet_only_commit,
        &receipt.placement,
    )?;
    validate_metrics(&receipt.metrics)?;
    validate_metric_parity(&receipt.capacity_metric_parity)?;
    validate_selection(
        &receipt.selection,
        &receipt.owner_loss,
        &receipt.second_owner_loss,
        &receipt.fallback_owner_loss,
        &receipt.fleet_only_commit,
        &receipt.follower_replacement,
        &receipt.placement,
    )?;
    validate_work(&receipt.work)
}

fn validate_selection(
    selection: &ClusterSelectionEvidence,
    owner_loss: &Value,
    second_owner_loss: &Value,
    fallback_owner_loss: &Value,
    fleet_only_commit: &Value,
    follower_replacement: &Value,
    placement: &Value,
) -> Result<()> {
    let owner_loss = as_object(owner_loss, "owner loss")?;
    let second_owner_loss = as_object(second_owner_loss, "second owner loss")?;
    let fallback_owner_loss = as_object(fallback_owner_loss, "fallback owner loss")?;
    validate_selection_cycle(
        &selection.owner_loss,
        session(owner_loss, "session_before")?,
        session(owner_loss, "session_after")?,
        true,
    )?;
    validate_selection_cycle(
        &selection.second_owner_loss,
        session(second_owner_loss, "session_before")?,
        session(second_owner_loss, "session_after")?,
        true,
    )?;
    validate_selection_cycle(
        &selection.fallback,
        session(fallback_owner_loss, "session_before")?,
        session(fallback_owner_loss, "session_after")?,
        false,
    )?;

    let placement = as_object(placement, "placement")?;
    let expected_failed =
        placement_node_for_session(placement, &selection.owner_loss.failed_session)?;
    let expected_successor =
        placement_node_for_session(placement, &selection.owner_loss.successor_session)?;
    if selection.owner_loss.failed_node != expected_failed
        || selection.owner_loss.successor_node != expected_successor
    {
        return Err(Error::Control("owner-loss selection identity"));
    }
    let first_members = member_nodes(as_object(
        object_value(
            as_object(fleet_only_commit, "fleet-only commit")?,
            "node_log_before",
        )?,
        "node log",
    )?)?;
    if !first_members
        .iter()
        .any(|member| *member == selection.owner_loss.successor_node)
        || first_members
            != selection
                .owner_loss
                .failed_log_members
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
    {
        return Err(Error::Control("owner-loss successor is not a follower"));
    }

    let second_members = member_nodes(as_object(
        object_value(
            as_object(follower_replacement, "follower replacement")?,
            "node_log_after",
        )?,
        "node log",
    )?)?;
    if selection.second_owner_loss.failed_node != selection.owner_loss.successor_node
        || selection.second_owner_loss.failed_node == selection.second_owner_loss.successor_node
        || !second_members
            .iter()
            .any(|member| *member == selection.second_owner_loss.successor_node)
        || second_members
            != selection
                .second_owner_loss
                .failed_log_members
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
    {
        return Err(Error::Control("second owner-loss selection identity"));
    }
    let fallback_log = as_object(
        object_value(fallback_owner_loss, "node_log_before")?,
        "fallback node log",
    )?;
    let fallback_candidate = as_object(
        object_value(fallback_owner_loss, "candidate_record")?,
        "fallback candidate",
    )?;
    let fallback_candidate_advertisement = as_object(
        object_value(fallback_candidate, "advertisement")?,
        "fallback candidate advertisement",
    )?;
    let fallback_members = member_nodes(fallback_log)?;
    if selection.fallback.failed_session != selection.second_owner_loss.successor_session
        || selection.fallback.failed_node != selection.second_owner_loss.successor_node
        || !boolean(fallback_candidate, "live")?
        || session(fallback_candidate, "session")? != selection.fallback.successor_session
        || node_id(fallback_candidate_advertisement, "node")? != selection.fallback.successor_node
        || fallback_members
            != selection
                .fallback
                .failed_log_members
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        || fallback_members
            .iter()
            .any(|member| *member == selection.fallback.successor_node)
    {
        return Err(Error::Control("fallback selection identity"));
    }
    Ok(())
}

fn validate_selection_cycle(
    cycle: &SelectionCycle,
    expected_failed_session: &str,
    expected_successor_session: &str,
    selected_original_follower: bool,
) -> Result<()> {
    validate_session_value(&cycle.failed_session)?;
    validate_session_value(&cycle.successor_session)?;
    validate_node_value(&cycle.failed_node)?;
    validate_node_value(&cycle.successor_node)?;
    if cycle.failed_session != expected_failed_session
        || cycle.successor_session != expected_successor_session
        || cycle.failed_session == cycle.successor_session
        || cycle.failed_node == cycle.successor_node
        || cycle.selected_original_follower != selected_original_follower
        || cycle.terminal_result != "succeeded"
        || cycle.failed_log_members.is_empty()
        || cycle
            .failed_log_members
            .iter()
            .any(|member| validate_node_value(member).is_err())
        || cycle
            .failed_log_members
            .windows(2)
            .any(|members| members[0] >= members[1])
    {
        return Err(Error::Control("cluster selection evidence"));
    }
    Ok(())
}

fn placement_node_for_session<'a>(
    placement: &'a Map<String, Value>,
    expected_session: &str,
) -> Result<&'a str> {
    for label in ["node_a", "node_b", "node_c", "node_d"] {
        let node = as_object(object_value(placement, label)?, "placement node")?;
        if session(node, "session")? == expected_session {
            return node_id(
                as_object(
                    object_value(node, "advertisement")?,
                    "placement advertisement",
                )?,
                "node",
            );
        }
    }
    Err(Error::Control("selection session is not in placement"))
}

fn member_nodes(object: &Map<String, Value>) -> Result<Vec<&str>> {
    object_value(object, "member_nodes")?
        .as_array()
        .ok_or(Error::Control("cluster receipt array"))?
        .iter()
        .map(|member| {
            let value = member
                .as_str()
                .ok_or(Error::Control("cluster receipt node"))?;
            validate_node_value(value)?;
            Ok(value)
        })
        .collect()
}

fn validate_work(work: &ClusterWorkEvidence) -> Result<()> {
    validate_work_cycle(&work.owner_loss, false)?;
    validate_work_cycle(&work.second_owner_loss, false)?;
    validate_work_cycle(&work.fallback, true)
}

fn validate_work_cycle(work: &WorkCycle, allow_object_only: bool) -> Result<()> {
    for value in [
        work.candidate_count,
        work.affected_cells,
        work.catalog_shards,
        work.catalog_pages,
        work.control_reads,
        work.bundle_bytes,
        work.object_reads,
        work.object_writes,
    ] {
        if value == 0 || value > MAX_CLUSTER_WORK_VALUE {
            return Err(Error::Control("cluster recovery work counter"));
        }
    }
    for value in [
        work.follower_pages,
        work.follower_frames,
        work.follower_bytes,
        work.peer_requests,
    ] {
        if value > MAX_CLUSTER_WORK_VALUE || (!allow_object_only && value == 0) {
            return Err(Error::Control("cluster recovery follower work counter"));
        }
    }
    let phases = [
        &work.phases.claim,
        &work.phases.witness,
        &work.phases.scope_validation,
        &work.phases.pin_attach,
        &work.phases.seal,
    ];
    for phase in phases {
        if phase.count == 0 || phase.duration_ms > MAX_CLUSTER_WORK_VALUE {
            return Err(Error::Control("cluster recovery phase evidence"));
        }
    }
    Ok(())
}

fn validate_owner_loss(value: &Value) -> Result<()> {
    let object = as_object(value, "owner loss")?;
    let before_session = session(object, "session_before")?;
    let after_session = session(object, "session_after")?;
    if before_session == after_session {
        return Err(Error::Control("owner loss did not change session"));
    }
    let before_epoch = number(object, "epoch_before")?;
    let after_epoch = number(object, "epoch_after")?;
    if after_epoch <= before_epoch {
        return Err(Error::Control("owner loss epoch did not advance"));
    }
    validate_timing(object)?;
    let before_root = root(object_value(object, "root_before")?)?;
    let restored_root = root(object_value(object, "root_after_restore")?)?;
    let continued_root = root(object_value(object, "root_continued")?)?;
    require_root_advance(before_root, restored_root)?;
    require_root_advance(restored_root, continued_root)
}

fn validate_fleet_only_commit(value: &Value) -> Result<()> {
    let object = as_object(value, "fleet-only commit")?;
    if number(object, "owner_uncovered_bytes")? == 0
        || number(object, "follower_retained_bytes")? == 0
        || !boolean(object, "immutable_object_put_rejected")?
        || !boolean(object, "owner_disk_removed_before_policy_restore")?
    {
        return Err(Error::Control("fleet-only durability evidence"));
    }
    let control = as_object(
        object_value(object, "control_before_owner_loss")?,
        "control",
    )?;
    if string(control, "state")? != "serving" {
        return Err(Error::Control("fleet-only control state"));
    }
    let lease = as_object(object_value(control, "owner_lease")?, "owner lease")?;
    if string(lease, "state")? != "live"
        || number(lease, "expires_at_ms")? <= number(lease, "observed_at_ms")?
    {
        return Err(Error::Control("fleet-only owner lease"));
    }
    if number(
        as_object(object_value(object, "response")?, "response")?,
        "id",
    )? != 1
        || array_len(
            as_object(object_value(object, "restored_labels")?, "restored labels")?,
            "items",
        )? != 1
    {
        return Err(Error::Control("fleet-only response evidence"));
    }
    Ok(())
}

fn validate_follower_replacement(value: &Value) -> Result<()> {
    let object = as_object(value, "follower replacement")?;
    let before = as_object(object_value(object, "node_log_before")?, "node log before")?;
    let after = as_object(object_value(object, "node_log_after")?, "node log after")?;
    if number(after, "epoch")? <= number(before, "epoch")? || array_len(after, "member_nodes")? != 1
    {
        return Err(Error::Control("follower replacement log"));
    }
    if number(
        as_object(
            object_value(object, "object_covered_response")?,
            "object response",
        )?,
        "number",
    )? != 3
        || number(
            as_object(
                object_value(object, "fleet_only_response")?,
                "fleet response",
            )?,
            "id",
        )? != 2
    {
        return Err(Error::Control("follower replacement response"));
    }
    Ok(())
}

fn validate_second_owner_loss(value: &Value) -> Result<()> {
    let object = as_object(value, "second owner loss")?;
    if session(object, "session_before")? == session(object, "session_after")? {
        return Err(Error::Control("second owner loss did not change session"));
    }
    if number(object, "epoch_after")? <= number(object, "epoch_before")? {
        return Err(Error::Control("second owner loss epoch did not advance"));
    }
    validate_timing(object)?;
    let before = root(object_value(object, "root_before")?)?;
    let after = root(object_value(object, "root_after")?)?;
    require_root_advance(before, after)?;
    if array_len(
        as_object(object_value(object, "restored_labels")?, "restored labels")?,
        "items",
    )? != 2
    {
        return Err(Error::Control("second owner loss labels"));
    }
    Ok(())
}

fn validate_fallback_owner_loss(value: &Value) -> Result<()> {
    let object = as_object(value, "fallback owner loss")?;
    if session(object, "session_before")? == session(object, "session_after")? {
        return Err(Error::Control("fallback owner loss did not change session"));
    }
    if number(object, "epoch_after")? <= number(object, "epoch_before")? {
        return Err(Error::Control("fallback owner loss epoch did not advance"));
    }
    validate_timing(object)?;
    let before = root(object_value(object, "root_before")?)?;
    let after = root(object_value(object, "root_after")?)?;
    require_root_advance(before, after)?;
    let log = as_object(
        object_value(object, "node_log_before")?,
        "fallback node log",
    )?;
    let members = member_nodes(log)?;
    if members.is_empty()
        || number(
            as_object(object_value(object, "response")?, "fallback response")?,
            "id",
        )? != 3
        || array_len(
            as_object(object_value(object, "restored_labels")?, "fallback labels")?,
            "items",
        )? != 3
    {
        return Err(Error::Control("fallback owner-loss evidence"));
    }
    Ok(())
}

fn validate_timing(object: &Map<String, Value>) -> Result<()> {
    let timing = as_object(object_value(object, "timing")?, "timing")?;
    let owner_killed = number(timing, "owner_killed_ms")?;
    let advertisement_expired = number(timing, "advertisement_expired_ms")?;
    let recovery_sealed = number(timing, "recovery_sealed_ms")?;
    let first_served = number(timing, "first_served_ms")?;
    if owner_killed == 0
        || owner_killed > advertisement_expired
        || advertisement_expired > recovery_sealed
        || recovery_sealed > first_served
    {
        return Err(Error::Control("cluster qualification timing"));
    }
    Ok(())
}

fn validate_capacity(value: &Value) -> Result<()> {
    let object = as_object(value, "capacity")?;
    for node in ["node_a", "node_b", "node_c", "node_d"] {
        let node = as_object(object_value(object, node)?, "capacity node")?;
        if number(node, "version")? != 1 {
            return Err(Error::Control("capacity schema"));
        }
        let resources = as_object(object_value(node, "resources")?, "capacity resources")?;
        if number(resources, "memory_bytes")? == 0 || number(resources, "free_disk_bytes")? == 0 {
            return Err(Error::Control("capacity resources"));
        }
        let admission = as_object(object_value(node, "admission")?, "capacity admission")?;
        if number(admission, "active_cells")? == 0 {
            return Err(Error::Control("capacity admission"));
        }
    }
    Ok(())
}

fn validate_measured_disk(value: &Value) -> Result<()> {
    let object = as_object(value, "measured disk")?;
    for node in [
        "node_a_bytes",
        "node_b_bytes",
        "node_c_bytes",
        "node_d_bytes",
        "tolerance_bytes",
    ] {
        number(object, node)?;
    }
    Ok(())
}

fn validate_placement(value: &Value) -> Result<()> {
    let object = as_object(value, "placement")?;
    for node in ["node_a", "node_b", "node_c", "node_d"] {
        let node = as_object(object_value(object, node)?, "placement node")?;
        if !boolean(node, "live")? {
            return Err(Error::Control("placement node is not live"));
        }
        let advertisement = as_object(
            object_value(node, "advertisement")?,
            "placement advertisement",
        )?;
        as_object(
            object_value(advertisement, "placement")?,
            "placement capacity",
        )?;
        as_object(object_value(advertisement, "log")?, "placement log")?;
    }
    Ok(())
}

fn validate_follower_affinity(
    owner_loss: &Value,
    fleet_only_commit: &Value,
    placement: &Value,
) -> Result<()> {
    let owner_loss = as_object(owner_loss, "owner loss")?;
    let failed_session = session(owner_loss, "session_before")?;
    let successor_session = session(owner_loss, "session_after")?;
    let fleet_only_commit = as_object(fleet_only_commit, "fleet-only commit")?;
    let placement = as_object(placement, "placement")?;
    let mut failed_node = None;
    let mut successor_node = None;
    for (label, session_key) in [
        ("node_a", "node_a_session"),
        ("node_b", "node_b_session"),
        ("node_c", "node_c_session"),
    ] {
        let placement_session = session(placement, session_key)?;
        let node = as_object(object_value(placement, label)?, "placement node")?;
        if session(node, "session")? != placement_session {
            return Err(Error::Control("placement session identity"));
        }
        let advertisement = as_object(
            object_value(node, "advertisement")?,
            "placement advertisement",
        )?;
        let node_id = node_id(advertisement, "node")?;
        if placement_session == failed_session {
            failed_node = Some(node_id);
        }
        if placement_session == successor_session {
            successor_node = Some(node_id);
        }
    }
    let failed_node = failed_node.ok_or(Error::Control("failed owner placement"))?;
    let successor_node = successor_node.ok_or(Error::Control("successor placement"))?;
    if failed_node == successor_node {
        return Err(Error::Control("successor reused failed owner node"));
    }

    let node_log = as_object(
        object_value(fleet_only_commit, "node_log_before")?,
        "node log",
    )?;
    let members = object_value(node_log, "member_nodes")?
        .as_array()
        .ok_or(Error::Control("cluster receipt array"))?;
    if !members
        .iter()
        .any(|member| member.as_str() == Some(successor_node))
    {
        return Err(Error::Control("successor was not an original follower"));
    }
    Ok(())
}

fn validate_metrics(value: &Value) -> Result<()> {
    let object = as_object(value, "metrics")?;
    for node in ["node_a", "node_b", "node_c", "node_d"] {
        let metrics = object_value(object, node)?
            .as_str()
            .ok_or(Error::Control("metrics payload"))?;
        for forbidden in [
            "node=\"",
            "session=\"",
            "cell=\"",
            "application=\"",
            "incarnation=\"",
            "owner=\"",
            "repository=\"",
            "tenant=\"",
            "bucket=\"",
            "node_id=\"",
        ] {
            if metrics.contains(forbidden) {
                return Err(Error::Control("metrics payload contains an identifier"));
            }
        }
        if metrics.is_empty() {
            return Err(Error::Control("metrics payload"));
        }
    }
    Ok(())
}

fn validate_metric_parity(value: &Value) -> Result<()> {
    let object = as_object(value, "capacity metric parity")?;
    for field in [
        "local_disk",
        "active_cells",
        "measured_local_disk",
        "signed_placement",
    ] {
        if !boolean(object, field)? {
            return Err(Error::Control("capacity metric parity"));
        }
    }
    Ok(())
}

fn require_root_advance(before: RootEvidence<'_>, after: RootEvidence<'_>) -> Result<()> {
    if before.digest == after.digest
        || after.txid <= before.txid
        || after.commit_sequence <= before.commit_sequence
    {
        return Err(Error::Control("recovered root did not advance"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct RootEvidence<'a> {
    digest: &'a str,
    txid: u64,
    commit_sequence: u64,
}

fn root(value: &Value) -> Result<RootEvidence<'_>> {
    let object = as_object(value, "root")?;
    let digest = string(object, "digest")?;
    if digest.len() != 64 || !digest.bytes().all(is_lower_hex) {
        return Err(Error::Control("root digest"));
    }
    let txid = number(object, "txid")?;
    let commit_sequence = number(object, "commit_sequence")?;
    if txid == 0 || commit_sequence == 0 {
        return Err(Error::Control("root watermark"));
    }
    Ok(RootEvidence {
        digest,
        txid,
        commit_sequence,
    })
}

fn validate_revision(value: &str) -> Result<()> {
    if value.len() != 40 || !value.bytes().all(is_lower_hex) {
        return Err(Error::Control("cluster qualification source revision"));
    }
    Ok(())
}

fn as_object<'a>(value: &'a Value, _name: &'static str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or(Error::Control("cluster receipt object"))
}

fn object_value<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a Value> {
    object
        .get(key)
        .ok_or(Error::Control("cluster receipt field"))
}

fn string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    object_value(object, key)?
        .as_str()
        .ok_or(Error::Control("cluster receipt string"))
}

fn session<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    let value = string(object, key)?;
    validate_session_value(value)?;
    Ok(value)
}

fn node_id<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    let value = string(object, key)?;
    validate_node_value(value)?;
    Ok(value)
}

fn validate_session_value(value: &str) -> Result<()> {
    if value.len() != 32 || !value.bytes().all(is_lower_hex) {
        return Err(Error::Control("cluster receipt session"));
    }
    Ok(())
}

fn validate_node_value(value: &str) -> Result<()> {
    if value.len() != 32 || !value.bytes().all(is_lower_hex) {
        return Err(Error::Control("cluster receipt node"));
    }
    Ok(())
}

fn number(object: &Map<String, Value>, key: &str) -> Result<u64> {
    object_value(object, key)?
        .as_u64()
        .ok_or(Error::Control("cluster receipt number"))
}

fn boolean(object: &Map<String, Value>, key: &str) -> Result<bool> {
    object_value(object, key)?
        .as_bool()
        .ok_or(Error::Control("cluster receipt boolean"))
}

fn array_len(object: &Map<String, Value>, key: &str) -> Result<usize> {
    object_value(object, key)?
        .as_array()
        .map(Vec::len)
        .ok_or(Error::Control("cluster receipt array"))
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        ClusterWorkEvidence, RecoveryPhaseEvidence, RecoveryPhaseTiming, WorkCycle,
        validate_follower_affinity, validate_metrics, validate_revision, validate_timing,
        validate_work_cycle,
    };

    #[test]
    fn source_revision_requires_a_lowercase_commit_shape() {
        assert!(validate_revision(&"a".repeat(40)).is_ok());
        assert!(validate_revision(&"A".repeat(40)).is_err());
        assert!(validate_revision("not-a-commit".repeat(4).as_str()).is_err());
    }

    #[test]
    fn timing_requires_monotonic_failover_boundaries() {
        let valid = json!({
            "timing": {
                "owner_killed_ms": 10,
                "advertisement_expired_ms": 20,
                "recovery_sealed_ms": 30,
                "first_served_ms": 40
            }
        });
        assert!(validate_timing(valid.as_object().unwrap()).is_ok());

        let invalid = json!({
            "timing": {
                "owner_killed_ms": 20,
                "advertisement_expired_ms": 10,
                "recovery_sealed_ms": 30,
                "first_served_ms": 40
            }
        });
        assert!(validate_timing(invalid.as_object().unwrap()).is_err());
    }

    #[test]
    fn follower_affinity_requires_a_member_of_the_failed_log() {
        let failed_session = "a".repeat(32);
        let successor_session = "b".repeat(32);
        let failed_node = "c".repeat(32);
        let successor_node = "d".repeat(32);
        let owner_loss = json!({
            "session_before": failed_session,
            "session_after": successor_session,
        });
        let fleet_only_commit = json!({
            "node_log_before": {"member_nodes": [successor_node]}
        });
        let placement = json!({
            "node_a_session": "e".repeat(32),
            "node_b_session": "a".repeat(32),
            "node_c_session": "b".repeat(32),
            "node_a": {"session": "e".repeat(32), "advertisement": {"node": "f".repeat(32)}},
            "node_b": {"session": "a".repeat(32), "advertisement": {"node": "c".repeat(32)}},
            "node_c": {"session": "b".repeat(32), "advertisement": {"node": "d".repeat(32)}},
            "node_d_session": "1".repeat(32),
            "node_d": {"session": "1".repeat(32), "advertisement": {"node": "2".repeat(32)}}
        });
        assert!(validate_follower_affinity(&owner_loss, &fleet_only_commit, &placement).is_ok());

        let mut invalid_fleet = fleet_only_commit.clone();
        invalid_fleet["node_log_before"]["member_nodes"] = json!([failed_node]);
        assert!(validate_follower_affinity(&owner_loss, &invalid_fleet, &placement).is_err());

        let mut mismatched_placement = placement.clone();
        mismatched_placement["node_c"]["session"] = json!("f".repeat(32));
        assert!(
            validate_follower_affinity(&owner_loss, &fleet_only_commit, &mismatched_placement)
                .is_err()
        );

        let mut reused_owner = placement;
        reused_owner["node_c"]["advertisement"]["node"] = json!(failed_node);
        assert!(
            validate_follower_affinity(&owner_loss, &fleet_only_commit, &reused_owner).is_err()
        );
    }

    #[test]
    fn work_evidence_requires_a_bounded_observation_per_phase() {
        let phase = RecoveryPhaseTiming {
            count: 1,
            duration_ms: 0,
        };
        let valid = WorkCycle {
            candidate_count: 1,
            affected_cells: 1,
            catalog_shards: 1,
            catalog_pages: 1,
            control_reads: 1,
            follower_pages: 1,
            follower_frames: 1,
            follower_bytes: 1,
            peer_requests: 1,
            bundle_bytes: 1,
            object_reads: 1,
            object_writes: 1,
            phases: RecoveryPhaseEvidence {
                claim: phase,
                witness: phase,
                scope_validation: phase,
                pin_attach: phase,
                seal: phase,
            },
        };
        assert!(validate_work_cycle(&valid, false).is_ok());
        let object_only = WorkCycle {
            follower_pages: 0,
            follower_frames: 0,
            follower_bytes: 0,
            peer_requests: 0,
            ..valid.clone()
        };
        assert!(validate_work_cycle(&object_only, true).is_ok());

        let invalid = WorkCycle {
            phases: RecoveryPhaseEvidence {
                claim: RecoveryPhaseTiming {
                    count: 0,
                    duration_ms: 0,
                },
                ..valid.phases
            },
            ..valid
        };
        assert!(validate_work_cycle(&invalid, false).is_err());
    }

    #[test]
    fn metrics_reject_identifier_bearing_labels() {
        let valid = json!({
            "node_a": "crab_cell_node_log_recovery_work_total{kind=\"candidate_count\"} 1",
            "node_b": "crab_cell_node_log_recovery_work_total{kind=\"candidate_count\"} 1",
            "node_c": "crab_cell_node_log_recovery_work_total{kind=\"candidate_count\"} 1",
            "node_d": "crab_cell_node_log_recovery_work_total{kind=\"candidate_count\"} 1"
        });
        assert!(validate_metrics(&valid).is_ok());
        let invalid = json!({
            "node_a": "crab_cell_node_log_recovery_work_total{node=\"deadbeef\"} 1",
            "node_b": "ok",
            "node_c": "ok",
            "node_d": "ok"
        });
        assert!(validate_metrics(&invalid).is_err());
    }

    #[test]
    fn work_receipt_shape_rejects_missing_or_unknown_phase_fields() {
        let phase = json!({"count": 1, "duration_ms": 0});
        let cycle = json!({
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
                "claim": phase,
                "witness": phase,
                "scope_validation": phase,
                "pin_attach": phase,
                "seal": phase
            }
        });
        let valid = json!({
            "owner_loss": cycle.clone(),
            "second_owner_loss": cycle.clone(),
            "fallback": cycle.clone()
        });
        assert!(serde_json::from_value::<ClusterWorkEvidence>(valid).is_ok());

        let mut missing = cycle.clone();
        missing["phases"].as_object_mut().unwrap().remove("seal");
        assert!(serde_json::from_value::<WorkCycle>(missing).is_err());

        let mut unknown = cycle;
        unknown["phases"]["seal"]["extra"] = json!(true);
        assert!(serde_json::from_value::<WorkCycle>(unknown).is_err());
    }
}
