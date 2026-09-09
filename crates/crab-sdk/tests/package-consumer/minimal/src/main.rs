fn main() -> Result<(), crab_sdk::Error> {
    let oid = crab_sdk::ObjectId::from_hex("0123456789012345678901234567890123456789")?;
    assert_eq!(oid.to_string(), "0123456789012345678901234567890123456789");
    Ok(())
}
