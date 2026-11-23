use gix_hash::ObjectId;

use crate::auth::Identity;

pub(crate) fn commit_bytes(
    tree: ObjectId,
    parents: &[ObjectId],
    actor: &Identity,
    message: &str,
    seconds: u64,
) -> Result<Vec<u8>, crab_remote::objects::Error> {
    let (name, email) = commit_identity(actor);
    let identity = crab_remote::objects::Signature {
        name: &name,
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

pub(crate) fn commit_identity(actor: &Identity) -> (String, String) {
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
    (name.to_owned(), email)
}

// Rewrite Git's exact author, encoding, and message while assigning the new
// committer. Existing signatures cover the old object and cannot be carried over.
pub(crate) fn rebased_commit_bytes(
    tree: ObjectId,
    parent: ObjectId,
    original: &[u8],
    actor: &Identity,
    seconds: u64,
) -> Result<Vec<u8>, crab_remote::objects::Error> {
    use gix_object::{CommitRef, WriteTo};
    let original = CommitRef::from_bytes(original, gix_hash::Kind::Sha1)?;
    let template = commit_bytes(tree, &[parent], actor, "", seconds)?;
    let mut rewritten = CommitRef::from_bytes(&template, gix_hash::Kind::Sha1)?;
    rewritten.author = original.author;
    rewritten.encoding = original.encoding;
    rewritten.message = original.message;
    let mut bytes = Vec::new();
    rewritten.write_to(&mut bytes)?;
    Ok(bytes)
}
