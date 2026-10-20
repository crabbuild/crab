use gix_hash::ObjectId;

use crate::auth::Identity;

pub(crate) fn commit_bytes(
    tree: ObjectId,
    parents: &[ObjectId],
    actor: &Identity,
    message: &str,
    seconds: u64,
) -> Result<Vec<u8>, crab_remote::objects::Error> {
    let name: String = actor
        .name
        .chars()
        .filter(|character| !matches!(character, '<' | '>' | '\n' | '\r' | '\0'))
        .take(160)
        .collect();
    let name = if name.trim().is_empty() {
        "Crab user"
    } else {
        name.trim()
    };
    let email_key = blake3::hash(format!("{}\0{}", actor.issuer, actor.subject).as_bytes());
    let email = format!("{}@users.crab.invalid", email_key.to_hex());
    let identity = crab_remote::objects::Signature {
        name,
        email: &email,
        seconds: i64::try_from(seconds).map_err(|_| {
            crab_remote::objects::Error::Identity("timestamp exceeds signed Git seconds")
        })?,
        offset_minutes: 0,
    };
    // HTTP commits retain the server's actor identity and trailing-newline policy.
    let mut bytes =
        crab_remote::objects::encode_commit(tree, parents, identity, identity, message.as_bytes())?;
    bytes.push(b'\n');
    Ok(bytes)
}
