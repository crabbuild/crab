//! Opaque GitHub node IDs and numeric REST IDs for Crab objects.
//!
//! Node IDs embed the stable repository UUID with the object's number, so a
//! mutation can never act on a different repository that later reuses the
//! same address. Clients treat them as opaque strings.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Discussion {
    Issue,
    Pull,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NodeId {
    Repository(Uuid),
    User(Uuid),
    Organization(Uuid),
    /// A team, under its organization.
    Team(Uuid, Uuid),
    Issue(Uuid, u64),
    Pull(Uuid, u64),
    Comment {
        repository: Uuid,
        discussion: Discussion,
        number: u64,
        comment: u64,
    },
    Label(Uuid, u64),
    Milestone(Uuid, u64),
    Review(Uuid, u64, u64),
    Commit(Uuid, String),
    Ref(Uuid, String),
}

const MAX_ID_BYTES: usize = 512;

impl NodeId {
    pub(crate) fn encode(&self) -> String {
        let mut payload = Vec::new();
        let prefix = match self {
            Self::Repository(id) => {
                payload.extend_from_slice(id.as_bytes());
                "R"
            }
            Self::User(id) => {
                payload.extend_from_slice(id.as_bytes());
                "U"
            }
            Self::Organization(id) => {
                payload.extend_from_slice(id.as_bytes());
                "O"
            }
            Self::Team(organization, team) => {
                payload.extend_from_slice(organization.as_bytes());
                payload.extend_from_slice(team.as_bytes());
                "T"
            }
            Self::Issue(repository, number) => {
                payload.extend_from_slice(repository.as_bytes());
                payload.extend_from_slice(&number.to_be_bytes());
                "I"
            }
            Self::Pull(repository, number) => {
                payload.extend_from_slice(repository.as_bytes());
                payload.extend_from_slice(&number.to_be_bytes());
                "PR"
            }
            Self::Comment {
                repository,
                discussion,
                number,
                comment,
            } => {
                payload.extend_from_slice(repository.as_bytes());
                payload.push(match discussion {
                    Discussion::Issue => 0,
                    Discussion::Pull => 1,
                });
                payload.extend_from_slice(&number.to_be_bytes());
                payload.extend_from_slice(&comment.to_be_bytes());
                "IC"
            }
            Self::Label(repository, number) => {
                payload.extend_from_slice(repository.as_bytes());
                payload.extend_from_slice(&number.to_be_bytes());
                "LA"
            }
            Self::Milestone(repository, number) => {
                payload.extend_from_slice(repository.as_bytes());
                payload.extend_from_slice(&number.to_be_bytes());
                "MI"
            }
            Self::Review(repository, pull, review) => {
                payload.extend_from_slice(repository.as_bytes());
                payload.extend_from_slice(&pull.to_be_bytes());
                payload.extend_from_slice(&review.to_be_bytes());
                "PRR"
            }
            Self::Commit(repository, oid) => {
                payload.extend_from_slice(repository.as_bytes());
                payload.extend_from_slice(oid.as_bytes());
                "C"
            }
            Self::Ref(repository, name) => {
                payload.extend_from_slice(repository.as_bytes());
                payload.extend_from_slice(name.as_bytes());
                "REF"
            }
        };
        format!("{prefix}_{}", URL_SAFE_NO_PAD.encode(payload))
    }

    pub(crate) fn decode(value: &str) -> Option<Self> {
        if value.len() > MAX_ID_BYTES {
            return None;
        }
        let (prefix, encoded) = value.split_once('_')?;
        let payload = URL_SAFE_NO_PAD.decode(encoded).ok()?;
        let uuid = |bytes: &[u8]| -> Option<Uuid> {
            Some(Uuid::from_bytes(bytes.get(..16)?.try_into().ok()?)).filter(|id| !id.is_nil())
        };
        let number = |bytes: &[u8], at: usize| -> Option<u64> {
            Some(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
        };
        let exact = |length: usize| (payload.len() == length).then_some(());
        Some(match prefix {
            "R" => {
                exact(16)?;
                Self::Repository(uuid(&payload)?)
            }
            "U" => {
                exact(16)?;
                Self::User(uuid(&payload)?)
            }
            "O" => {
                exact(16)?;
                Self::Organization(uuid(&payload)?)
            }
            "T" => {
                exact(32)?;
                Self::Team(uuid(&payload)?, uuid(&payload[16..])?)
            }
            "I" | "PR" | "LA" | "MI" => {
                exact(24)?;
                let (repository, number) = (uuid(&payload)?, number(&payload, 16)?);
                match prefix {
                    "I" => Self::Issue(repository, number),
                    "PR" => Self::Pull(repository, number),
                    "LA" => Self::Label(repository, number),
                    _ => Self::Milestone(repository, number),
                }
            }
            "IC" => {
                exact(33)?;
                Self::Comment {
                    repository: uuid(&payload)?,
                    discussion: match payload[16] {
                        0 => Discussion::Issue,
                        1 => Discussion::Pull,
                        _ => return None,
                    },
                    number: number(&payload, 17)?,
                    comment: number(&payload, 25)?,
                }
            }
            "PRR" => {
                exact(32)?;
                Self::Review(
                    uuid(&payload)?,
                    number(&payload, 16)?,
                    number(&payload, 24)?,
                )
            }
            "C" | "REF" => {
                let repository = uuid(&payload)?;
                let text = String::from_utf8(payload.get(16..)?.to_vec()).ok()?;
                if prefix == "C" {
                    Self::Commit(repository, text)
                } else {
                    Self::Ref(repository, text)
                }
            }
            _ => return None,
        })
    }

    /// The repository that owns this object, if it is repository-scoped.
    pub(crate) fn repository(&self) -> Option<Uuid> {
        match self {
            Self::Repository(id)
            | Self::Issue(id, _)
            | Self::Pull(id, _)
            | Self::Label(id, _)
            | Self::Milestone(id, _)
            | Self::Review(id, _, _)
            | Self::Commit(id, _)
            | Self::Ref(id, _)
            | Self::Comment { repository: id, .. } => Some(*id),
            Self::User(_) | Self::Organization(_) | Self::Team(..) => None,
        }
    }
}

/// A stable, JavaScript-safe REST `id` for an object without its own number.
pub(crate) fn rest_id(node: &NodeId) -> u64 {
    if let NodeId::Comment {
        discussion,
        number,
        comment,
        ..
    } = node
        && let Some(id) = comment_rest_id(*discussion, *number, *comment)
    {
        return id;
    }
    let digest = blake3::hash(node.encode().as_bytes());
    let bytes: [u8; 8] = digest.as_bytes()[..8]
        .try_into()
        .unwrap_or([1, 0, 0, 0, 0, 0, 0, 0]);
    (u64::from_le_bytes(bytes) >> 11).max(1)
}

/// GraphQL `databaseId` is a 32-bit `Int`; GitHub clients only display it.
pub(crate) fn database_id(node: &NodeId) -> i64 {
    i64::try_from(rest_id(node) & 0x7fff_ffff)
        .unwrap_or(1)
        .max(1)
}

const COMMENT_BITS: u32 = 24;
const NUMBER_BITS: u32 = 28;

/// REST addresses issue comments by a repository-scoped integer, so their ID
/// packs the discussion kind and both numbers instead of hashing them.
pub(crate) fn comment_rest_id(discussion: Discussion, number: u64, comment: u64) -> Option<u64> {
    if number >= 1 << NUMBER_BITS || comment >= 1 << COMMENT_BITS {
        return None;
    }
    let kind = match discussion {
        Discussion::Issue => 0,
        Discussion::Pull => 1,
    };
    Some((kind << (NUMBER_BITS + COMMENT_BITS)) | (number << COMMENT_BITS) | comment)
}

pub(crate) fn decode_comment_rest_id(id: u64) -> Option<(Discussion, u64, u64)> {
    let discussion = match id >> (NUMBER_BITS + COMMENT_BITS) {
        0 => Discussion::Issue,
        1 => Discussion::Pull,
        _ => return None,
    };
    let number = (id >> COMMENT_BITS) & ((1 << NUMBER_BITS) - 1);
    let comment = id & ((1 << COMMENT_BITS) - 1);
    (number > 0 && comment > 0).then_some((discussion, number, comment))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_ids_round_trip_and_reject_tampering() {
        let repository = Uuid::now_v7();
        for node in [
            NodeId::Repository(repository),
            NodeId::User(Uuid::now_v7()),
            NodeId::Organization(Uuid::now_v7()),
            NodeId::Team(Uuid::now_v7(), Uuid::now_v7()),
            NodeId::Issue(repository, 7),
            NodeId::Pull(repository, u64::MAX),
            NodeId::Comment {
                repository,
                discussion: Discussion::Pull,
                number: 3,
                comment: 9,
            },
            NodeId::Label(repository, 2),
            NodeId::Milestone(repository, 4),
            NodeId::Review(repository, 3, 11),
            NodeId::Commit(repository, "a".repeat(40)),
            NodeId::Ref(repository, "refs/heads/main".into()),
        ] {
            let encoded = node.encode();
            assert_eq!(NodeId::decode(&encoded), Some(node), "{encoded}");
        }
        let issue = NodeId::Issue(repository, 7).encode();
        for invalid in [
            "",
            "I_",
            "X_AAAA",
            &issue[..issue.len() - 2],
            &issue.replacen("I_", "R_", 1),
            &format!("R_{}", URL_SAFE_NO_PAD.encode([0; 16])),
        ] {
            assert_eq!(NodeId::decode(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn numeric_ids_are_javascript_safe_and_comment_ids_invert() {
        let node = NodeId::Issue(Uuid::now_v7(), 1);
        assert!(rest_id(&node) < 1 << 53 && rest_id(&node) > 0);
        assert!((1..=i64::from(i32::MAX)).contains(&database_id(&node)));
        let id = comment_rest_id(Discussion::Pull, 42, 7).unwrap();
        assert!(id < 1 << 53);
        assert_eq!(decode_comment_rest_id(id), Some((Discussion::Pull, 42, 7)));
        assert_eq!(comment_rest_id(Discussion::Issue, 1 << 28, 1), None);
        assert_eq!(decode_comment_rest_id(0), None);
    }
}
