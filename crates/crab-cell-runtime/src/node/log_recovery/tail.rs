//! Streaming one follower's tail through the common framing checks.
//!
//! A transport or malformed-page failure returns false so another complete
//! witness may be tried; digest disagreement stays a hard recovery error.

use super::*;

/// Streams one follower tail through the common page/framing checks. A
/// transport or malformed-page failure returns `false` so another complete
/// witness may be tried; digest disagreement remains a hard recovery error.
pub(super) struct TailReadContext<'a> {
    pub(super) transport: &'a dyn NodeLogTransport,
    pub(super) leader_session: SessionId,
    pub(super) log_epoch: u64,
    pub(super) required_first: u64,
    pub(super) limits: crab_ltx::Limits,
    pub(super) reservation: Option<&'a crab_ltx::DiskReservation>,
    pub(super) work: Option<&'a mut RecoveryWorkSummary>,
}

pub(super) struct TailSinks<'a> {
    pub(super) collector: Option<&'a mut WitnessCollector>,
    pub(super) digests: Option<&'a mut WitnessDigestWriter>,
    pub(super) compare: Option<&'a mut SealedWitnessDigests>,
}

pub(super) async fn collect_member_tail(
    member: NodeId,
    receipt: FollowerReceipt,
    context: TailReadContext<'_>,
    mut sinks: TailSinks<'_>,
) -> Result<bool> {
    let TailReadContext {
        transport,
        leader_session,
        log_epoch,
        required_first,
        limits,
        reservation,
        mut work,
    } = context;
    if receipt.durable_through < required_first {
        return Ok(false);
    }
    let mut first_sequence = required_first.max(receipt.base_sequence);
    if first_sequence > receipt.durable_through {
        return Ok(false);
    }
    let mut tail_bytes = 0_u64;
    loop {
        if let Some(work) = work.as_deref_mut() {
            work.peer_requests = work
                .peer_requests
                .checked_add(1)
                .ok_or(Error::Capacity("recovery peer request count"))?;
        }
        let page = match transport
            .tail_page(
                member,
                TailRequest {
                    leader_session,
                    log_epoch,
                    first_sequence,
                },
            )
            .await
        {
            Ok(page) => page,
            Err(_) => return Ok(false),
        };
        if page.frames.is_empty() || page.frames.len() > MAX_RECOVERY_PAGE_FRAMES {
            return Ok(false);
        }
        let page_count = page.frames.len();
        if let Some(work) = work.as_deref_mut() {
            work.follower_pages = work
                .follower_pages
                .checked_add(1)
                .ok_or(Error::Capacity("recovery follower page count"))?;
        }
        let verified = match page
            .frames
            .into_iter()
            .map(|bytes| crab_ltx::inspect_node_frame(bytes, limits))
            .collect::<crab_ltx::Result<Vec<_>>>()
        {
            Ok(verified) => verified,
            Err(_) => return Ok(false),
        };
        if verified.iter().enumerate().any(|(offset, frame)| {
            let scope = frame.scope();
            scope.leader_session != *leader_session.as_bytes()
                || scope.log_epoch != log_epoch
                || first_sequence.checked_add(offset as u64) != Some(scope.node_sequence)
                || scope.node_sequence > receipt.durable_through
        }) {
            return Ok(false);
        }
        let Some(page_bytes) = verified.iter().try_fold(0_u64, |bytes, frame| {
            bytes.checked_add(frame.encoded().len() as u64)
        }) else {
            return Ok(false);
        };
        if let Some(work) = work.as_deref_mut() {
            work.follower_frames = work
                .follower_frames
                .checked_add(
                    u64::try_from(verified.len())
                        .map_err(|_| Error::Capacity("recovery follower frame count"))?,
                )
                .ok_or(Error::Capacity("recovery follower frame count"))?;
            work.follower_bytes = work
                .follower_bytes
                .checked_add(page_bytes)
                .ok_or(Error::Capacity("recovery follower byte count"))?;
        }
        let page_limit = if page_count == 1 {
            MAX_RECOVERY_PAGE_BYTES.saturating_add(limits.max_capture_bytes)
        } else {
            MAX_RECOVERY_PAGE_BYTES
        };
        if page_bytes > page_limit {
            return Ok(false);
        }
        tail_bytes = match tail_bytes.checked_add(page_bytes) {
            Some(bytes) if bytes <= recovery_tail_reservation_bytes(limits) => bytes,
            _ => return Ok(false),
        };
        if let Some(reservation) = reservation {
            reservation.try_grow(page_bytes)?;
        }
        for frame in &verified {
            let sequence = frame.scope().node_sequence;
            let digest = frame.digest();
            if let Some(writer) = sinks.digests.as_mut() {
                writer.push(sequence, digest)?;
            }
            if let Some(table) = sinks.compare.as_mut() {
                table.matches(sequence, digest)?;
            }
        }
        let last_sequence = verified.last().map(|frame| frame.scope().node_sequence);
        if let Some(output) = sinks.collector.as_mut() {
            output.push(verified)?;
        }
        let Some(next_sequence) = page.next_sequence else {
            return Ok(last_sequence == Some(receipt.durable_through));
        };
        let page_count = u64::try_from(page_count)
            .map_err(|_| Error::Node("recovery page frame count overflows"))?;
        let expected_next = first_sequence
            .checked_add(page_count)
            .ok_or(Error::Node("recovery page sequence overflows"))?;
        if next_sequence != expected_next || next_sequence > receipt.durable_through {
            return Ok(false);
        }
        first_sequence = next_sequence;
    }
}

pub(super) fn default_recovery_disk(limits: crab_ltx::Limits) -> crab_ltx::DiskBudget {
    crab_ltx::DiskBudget::new(recovery_tail_reservation_bytes(limits))
}

fn recovery_tail_reservation_bytes(limits: crab_ltx::Limits) -> u64 {
    limits.max_plan_bytes.min(512 << 20)
}
