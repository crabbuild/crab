use super::*;

#[test]
fn maximum_check_results_fit_their_registered_output_bounds() {
    let target_prefix = "https://example.test/";
    let run = CheckRunRecord {
        number: 1,
        create_submission_id: [1; 16],
        author: RepositoryAuthor {
            issuer: "🦀".repeat(512),
            subject: "🦀".repeat(512),
            name: "🦀".repeat(160),
        },
        oid: "0123456789abcdef0123456789abcdef01234567".into(),
        name: "🦀".repeat(100),
        status: 1,
        conclusion: None,
        details_url: Some(format!(
            "{target_prefix}{}",
            "a".repeat(2_048 - target_prefix.len())
        )),
        output_title: "🦀".repeat(200),
        version: 1,
        started_at_ms: Some(1),
        completed_at_ms: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    let page = CheckRunPage {
        runs: vec![run.clone(); 100],
        next: None,
    };
    assert!(serde_json::to_vec(&page).unwrap().len() <= 1024 * 1024);

    let mut escaped = run.clone();
    // Exercise the worst JSON escaping at the stored target URL limit.
    escaped.details_url = Some("\\".repeat(2_048));
    let status = CommitStatusRecord {
        number: 1,
        submission_id: [1; 16],
        author: escaped.author.clone(),
        oid: escaped.oid.clone(),
        context: escaped.name.clone(),
        state: 3,
        description: Some("🦀".repeat(140)),
        target_url: escaped.details_url.clone(),
        created_at_ms: 1,
    };
    let reports = |include_evidence| required::Reports {
        statuses: (0..MAX_STATUS_CONTEXTS)
            .map(|_| required::Status::from_record(status.clone(), include_evidence))
            .collect(),
        runs: (0..MAX_CHECK_RUNS_PER_COMMIT)
            .map(|_| required::Run::from_record(escaped.clone(), include_evidence))
            .collect(),
        connections: (0..16)
            .map(|_| integrations::RequiredConnection {
                id: uuid::Uuid::nil(),
                name: escaped.name.clone(),
                run: Some(required::Run::from_record(
                    escaped.clone(),
                    include_evidence,
                )),
            })
            .collect(),
    };
    let snapshot = required::Snapshot {
        head: reports(false),
        candidate: Some(reports(true)),
    };
    assert!(
        serde_json::to_vec(&snapshot).unwrap().len()
            < crate::cells::application::MAX_RESULT_BYTES - 256
    );

    let detail = CheckRunDetail {
        run,
        output: CheckOutputRecord {
            title: "🦀".repeat(200),
            summary: "s".repeat(32 * 1024),
            text: Some("t".repeat(64 * 1024)),
            steps: (0..10)
                .map(|index| CheckStepRecord {
                    name: format!("step-{index}"),
                    status: 1,
                    conclusion: None,
                    log: Some("l".repeat(8 * 1024)),
                })
                .collect(),
            annotations: vec![],
        },
    };
    assert!(serde_json::to_vec(&detail).unwrap().len() <= 256 * 1024);
}

#[test]
fn check_output_reopens_the_existing_record_format() {
    let fixture = "0000000174000000016d000000000000000000";
    let bytes = (0..fixture.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&fixture[index..index + 2], 16).unwrap())
        .collect::<Vec<_>>();
    let mut decoder = RecordDecoder::new(&bytes, 256 * 1024).unwrap();
    let output = CheckOutputRecord::decode(&mut decoder).unwrap();
    decoder.finish().unwrap();
    let expected = CheckOutputRecord {
        title: "t".into(),
        summary: "m".into(),
        text: None,
        steps: vec![],
        annotations: vec![],
    };
    assert_eq!(output, expected);
    let mut encoder = RecordEncoder::new(256 * 1024).unwrap();
    output.encode(&mut encoder).unwrap();
    assert_eq!(encoder.finish(), bytes);
}
