use super::*;

#[test]
fn branch_protections_reopen_the_existing_record_format() {
    let fixture = "00000001000000046d61696e0200000001000000026369";
    let bytes = (0..fixture.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&fixture[index..index + 2], 16).unwrap())
        .collect::<Vec<_>>();
    let mut decoder = RecordDecoder::new(&bytes, 64 * 1024).unwrap();
    assert_eq!(decoder.read_count().unwrap(), 1);
    let rule = BranchProtectionRecord::decode(&mut decoder).unwrap();
    decoder.finish().unwrap();
    assert_eq!(
        rule,
        BranchProtectionRecord {
            branch: "main".into(),
            required_approvals: 2,
            required_code_owner_reviews: false,
            require_last_push_approval: false,
            require_conversation_resolution: false,
            require_up_to_date: false,
            require_merge_queue: false,
            require_linear_history: false,
            required_checks: vec!["ci".into()],
            required_integrations: vec![],
        }
    );
}

#[test]
fn branch_protection_edits_preserve_earlier_record_formats() {
    let mut rules = vec![
        BranchProtectionRecord {
            branch: "main".into(),
            required_approvals: 2,
            required_code_owner_reviews: true,
            require_last_push_approval: true,
            require_conversation_resolution: true,
            require_up_to_date: true,
            require_merge_queue: true,
            require_linear_history: true,
            required_checks: vec!["ci".into()],
            required_integrations: vec![uuid::Uuid::from_bytes([1; 16])],
        },
        BranchProtectionRecord {
            branch: "stable".into(),
            required_approvals: 0,
            required_code_owner_reviews: false,
            require_last_push_approval: false,
            require_conversation_resolution: false,
            require_up_to_date: false,
            require_merge_queue: false,
            require_linear_history: false,
            required_checks: Vec::new(),
            required_integrations: Vec::new(),
        },
    ];
    assert_eq!(
        codec::decode(&codec::encode(&rules).unwrap()).unwrap(),
        rules
    );

    rules[0].require_last_push_approval = false;
    let previous = codec::encode(&rules).unwrap();
    assert_eq!(&previous[..4], &106_u32.to_be_bytes());
    assert_eq!(codec::decode(&previous).unwrap(), rules);

    rules[0].require_linear_history = false;
    let previous = codec::encode(&rules).unwrap();
    assert_eq!(&previous[..4], &105_u32.to_be_bytes());
    assert_eq!(codec::decode(&previous).unwrap(), rules);

    rules[0].require_merge_queue = false;
    let previous = codec::encode(&rules).unwrap();
    assert_eq!(&previous[..4], &104_u32.to_be_bytes());
    assert_eq!(codec::decode(&previous).unwrap(), rules);

    rules[0].require_up_to_date = false;
    let previous = codec::encode(&rules).unwrap();
    assert_eq!(&previous[..4], &103_u32.to_be_bytes());
    assert_eq!(codec::decode(&previous).unwrap(), rules);

    rules[0].require_conversation_resolution = false;
    let previous = codec::encode(&rules).unwrap();
    assert_eq!(&previous[..4], &102_u32.to_be_bytes());
    assert_eq!(codec::decode(&previous).unwrap(), rules);
}
