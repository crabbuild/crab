use super::*;

#[test]
fn maximum_release_fits_the_registered_output_bound() {
    let author = RepositoryAuthor {
        issuer: "🦀".repeat(512),
        subject: "🦀".repeat(512),
        name: "🦀".repeat(160),
    };
    let release = ReleaseRecord {
        number: 1,
        create_submission_id: [9; 16],
        author: author.clone(),
        tag_name: "v1.0.0".into(),
        tag_oid: Some("0123456789abcdef0123456789abcdef01234567".into()),
        target_oid: "0123456789abcdef0123456789abcdef01234567".into(),
        title: "🦀".repeat(256),
        body: "\u{1}".repeat(64 * 1024),
        prerelease: false,
        draft: false,
        publication_pending: None,
        version: 97,
        created_at_ms: 1,
        published_at_ms: Some(1),
        updated_at_ms: 97,
        deleted: false,
        assets: (0..96)
            .map(|index| ReleaseAssetRecord {
                request_id: [index; 16],
                name: format!("asset-{index:03}"),
                content_type: "application/octet-stream".into(),
                size: 512 * 1024 * 1024,
                digest: "a".repeat(64),
                uploader: author.clone(),
                created_at_ms: 1,
            })
            .collect(),
    };
    assert!(serde_json::to_vec(&release).unwrap().len() <= 1024 * 1024);
}
