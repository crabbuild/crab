use gix_hash::ObjectId;
use gix_object::{Kind, WriteTo, tree};

use crate::auth::Identity;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("repository read failed")]
    Remote(#[from] crab_remote_git::Error),
    #[error("Git object decoding failed")]
    Decode(#[from] gix_object::decode::Error),
    #[error("Git object encoding failed")]
    Io(#[from] std::io::Error),
    #[error("Git object hashing failed")]
    Hash(#[from] gix_hash::hasher::Error),
}

pub(crate) async fn read_tree(
    operation: &crab_remote_git::OperationContext,
    oid: ObjectId,
) -> Result<Vec<tree::Entry>, Error> {
    let object = operation.read_object(oid).await?;
    if object.kind != Kind::Tree {
        return Err(crab_remote_git::Error::InternalInvariant {
            invariant: "commit tree path resolved to a non-tree object",
        }
        .into());
    }
    gix_object::TreeRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
        .map(gix_object::TreeRef::into_owned)
        .map(|tree| tree.entries)
        .map_err(Error::from)
}

pub(crate) fn encode_tree(
    mut entries: Vec<tree::Entry>,
    objects: &mut Vec<(Kind, Vec<u8>)>,
) -> Result<ObjectId, Error> {
    entries.sort();
    let tree = gix_object::Tree { entries };
    let mut bytes = Vec::new();
    tree.write_to(&mut bytes)?;
    let oid = object_id(Kind::Tree, &bytes)?;
    objects.push((Kind::Tree, bytes));
    Ok(oid)
}

pub(crate) fn object_id(kind: Kind, bytes: &[u8]) -> Result<ObjectId, gix_hash::hasher::Error> {
    gix_object::compute_hash(gix_hash::Kind::Sha1, kind, bytes)
}

pub(crate) fn commit_bytes(
    tree: ObjectId,
    parents: &[ObjectId],
    actor: &Identity,
    message: &str,
    seconds: u64,
) -> Vec<u8> {
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
    let parents = parents
        .iter()
        .map(|parent| format!("parent {parent}\n"))
        .collect::<String>();
    format!(
        "tree {tree}\n{parents}author {name} <{}@users.crab.invalid> {seconds} +0000\ncommitter {name} <{}@users.crab.invalid> {seconds} +0000\n\n{message}\n",
        email_key.to_hex(),
        email_key.to_hex(),
    )
    .into_bytes()
}
