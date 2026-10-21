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
