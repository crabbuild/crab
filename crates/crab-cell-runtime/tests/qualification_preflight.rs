#[test]
fn production_hot_paths_use_singleton_or_indexed_work() {
    let queue = include_str!("../src/queue.rs");
    let queue_info = queue
        .split_once("pub fn queue_info")
        .and_then(|(_, tail)| tail.split_once("pub fn verify_queue_counts"))
        .map(|(body, _)| body)
        .expect("queue info function is present");
    assert!(!queue_info.contains("count("));
    assert!(queue_info.contains("ready_count"));

    let workflow = include_str!("../src/workflow.rs");
    let next_sequence = workflow
        .split_once("fn next_sequence")
        .and_then(|(_, tail)| tail.split_once("pub fn verify_workflow_event_count"))
        .map(|(body, _)| body)
        .expect("workflow sequence allocator is present");
    assert!(!next_sequence.contains("count("));
    assert!(next_sequence.contains("event_count"));

    let effects = include_str!("../src/effects.rs");
    let insertion = effects
        .split_once("fn effect_insert")
        .and_then(|(_, tail)| tail.split_once("fn effect_operation"))
        .map(|(body, _)| body)
        .expect("effect insertion function is present");
    assert!(!insertion.contains("SELECT count"));
    assert!(effects.contains("operation_bytes"));
}

#[test]
fn qualification_workload_and_profile_are_reproducible() {
    use crab_cell_runtime::{QualificationProfile, QualificationWorkload};

    let profile = QualificationProfile::pr_contract();
    let workload = QualificationWorkload::generate(&profile, 17).expect("workload generation");
    workload
        .verify_for_profile(&profile)
        .expect("workload identity");
    assert_eq!(
        workload.profile_digest(),
        profile.digest().expect("profile digest")
    );
    assert_eq!(
        QualificationWorkload::decode(&workload.encode().expect("workload encoding"))
            .expect("workload decoding"),
        workload
    );
}
