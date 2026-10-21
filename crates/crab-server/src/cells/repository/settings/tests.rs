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
            required_checks: vec!["ci".into()]
        }
    );
}
