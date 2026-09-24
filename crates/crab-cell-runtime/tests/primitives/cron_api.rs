use crab_cell_runtime::primitives::cron::CronInvocation;
use crab_cell_runtime::primitives::cron::{CronMutation, CronQuery};

use crate::support::fixtures::codec_roundtrip;

#[test]
fn cron_codecs_roundtrip_schedule_and_invocation() {
    codec_roundtrip(CronMutation::Upsert {
        schedule_id: [1; 16],
        target_index: 2,
        target_partition: b"shard".to_vec(),
        payload: b"run".to_vec(),
        interval_ms: 1_000,
        next_due_ms: 10,
    });
    codec_roundtrip(CronInvocation {
        schedule_id: [1; 16],
        generation: 2,
        occurrence: 3,
        scheduled_at_ms: 10,
        payload: b"run".to_vec(),
    });
    codec_roundtrip(CronQuery::List {
        after: Some([2; 16]),
        limit: 128,
    });
}
