use super::*;

#[test]
fn maximum_utf8_status_catalog_fits_its_registered_output_bound() {
    let target_prefix = "https://example.test/";
    let status = CommitStatusRecord {
        number: 1,
        submission_id: [1; 16],
        author: RepositoryAuthor {
            issuer: "🦀".repeat(512),
            subject: "🦀".repeat(512),
            name: "🦀".repeat(160),
        },
        oid: "0123456789abcdef0123456789abcdef01234567".into(),
        context: "🦀".repeat(100),
        state: 3,
        description: Some("🦀".repeat(140)),
        target_url: Some(format!(
            "{target_prefix}{}",
            "a".repeat(2_048 - target_prefix.len())
        )),
        created_at_ms: 1,
    };
    let catalog = CommitStatusCatalog {
        statuses: vec![status; 128],
    };
    assert!(serde_json::to_vec(&catalog).unwrap().len() <= 1024 * 1024);
}
