//! Selection and placement evidence inside a cluster receipt.
//!
//! The receipt binds one signed owner session to the placement it was
//! elected from, so these checks re-derive which node the placement named
//! before any later phase is allowed to trust the selection.

use super::*;

pub(super) fn validate_selection(
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
            "node_log_after",
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
