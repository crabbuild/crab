use super::*;

#[test]
fn maximum_utf8_label_catalog_fits_its_registered_output_bound() {
    let label = LabelRecord {
        number: 1,
        name: "🦀".repeat(50),
        color: "123abc".into(),
        description: Some("🦀".repeat(100)),
        version: 1,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    let catalog = LabelCatalog {
        labels: vec![label; 500],
    };
    assert!(serde_json::to_vec(&catalog).unwrap().len() <= 384 * 1024);
}
