use crab_cell_runtime::*;
fn roundtrip<T: WireValue + PartialEq + std::fmt::Debug>(value: T) {
    let mut encoder = BoundedEncoder::new(1024 * 1024).unwrap();
    value.encode(&mut encoder).unwrap();
    let bytes = encoder.finish();
    let mut decoder = BoundedDecoder::new(&bytes, 1024 * 1024).unwrap();
    assert_eq!(T::decode(&mut decoder).unwrap(), value);
    decoder.finish().unwrap();
}
#[test]
fn cron_codecs_roundtrip_schedule_and_invocation() {
    roundtrip(CronMutation::Upsert {
        schedule_id: [1; 16],
        target_index: 2,
        target_partition: b"shard".to_vec(),
        payload: b"run".to_vec(),
        interval_ms: 1_000,
        next_due_ms: 10,
    });
    roundtrip(CronInvocation {
        schedule_id: [1; 16],
        generation: 2,
        occurrence: 3,
        scheduled_at_ms: 10,
        payload: b"run".to_vec(),
    });
    roundtrip(CronQuery::List {
        after: Some([2; 16]),
        limit: 128,
    });
}
