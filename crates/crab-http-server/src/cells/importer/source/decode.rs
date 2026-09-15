use bytes::Bytes;
use uuid::Uuid;

use crate::cells::repository::{
    CommentRecord, CreateCommentInput, CreateIssueInput, IssueRecord, RepositoryAuthor,
    comment_submission_digest, issue_submission_digest,
};
use crate::issues::storage::{Comment, Issue, IssueState};

use super::{validate_comment_v1, validate_issue_v1};

#[derive(Clone, Copy)]
pub(super) enum SourceKind {
    IssueSequence,
    IssueReservation,
    Issue,
    CommentSequence { issue: u64 },
    CommentReservation { issue: u64 },
    Comment { issue: u64, number: u64 },
}

impl SourceKind {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::IssueSequence => "issue_sequence",
            Self::IssueReservation => "issue_reservation",
            Self::Issue => "issue",
            Self::CommentSequence { .. } => "comment_sequence",
            Self::CommentReservation { .. } => "comment_reservation",
            Self::Comment { .. } => "comment",
        }
    }
}

pub(super) enum StageRecord {
    IssueSequence(u64),
    IssueReservation {
        request: [u8; 16],
        record: IssueRecord,
        digest: [u8; 32],
    },
    Issue {
        request: [u8; 16],
        record: IssueRecord,
    },
    CommentSequence {
        issue: u64,
        last: u64,
    },
    CommentReservation {
        request: [u8; 16],
        record: CommentRecord,
        digest: [u8; 32],
    },
    Comment {
        request: [u8; 16],
        record: CommentRecord,
    },
}

pub(super) fn classify(relative: &str) -> crate::Result<SourceKind> {
    let parts = relative.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        ["sequence.json"] => Ok(SourceKind::IssueSequence),
        ["requests", request] => {
            parse_request_filename(request)?;
            Ok(SourceKind::IssueReservation)
        }
        [issue, "issue.json"] => {
            parse_number(issue)?;
            Ok(SourceKind::Issue)
        }
        [issue, "comments", "sequence.json"] => Ok(SourceKind::CommentSequence {
            issue: parse_number(issue)?,
        }),
        [issue, "comments", "requests", request] => {
            parse_request_filename(request)?;
            Ok(SourceKind::CommentReservation {
                issue: parse_number(issue)?,
            })
        }
        [issue, "comments", comment] => Ok(SourceKind::Comment {
            issue: parse_number(issue)?,
            number: parse_number_filename(comment)?,
        }),
        _ => Err(crate::Error::Config(
            "legacy issue source contains an unknown object",
        )),
    }
}

pub(super) fn decode_record(
    kind: SourceKind,
    relative: &str,
    body: &Bytes,
) -> crate::Result<StageRecord> {
    match kind {
        SourceKind::IssueSequence => Ok(StageRecord::IssueSequence(
            crate::app_storage::decode_sequence(body)?,
        )),
        SourceKind::IssueReservation => {
            let legacy = crate::app_storage::decode::<Issue>(body)?;
            let request = request_id(&legacy.request_id, request_from_relative(relative)?)?;
            let record = issue_record(legacy)?;
            if record.state != 0
                || !record.label_ids.is_empty()
                || !record.assignee_subjects.is_empty()
                || record.version != 1
                || record.updated_at_ms != record.created_at_ms
            {
                return Err(crate::Error::Config(
                    "legacy issue reservation is not an initial proposal",
                ));
            }
            let digest = *issue_submission_digest(&CreateIssueInput {
                submission_id: request,
                author: record.author.clone(),
                title: record.title.clone(),
                body: record.body.clone(),
            })
            .as_bytes();
            Ok(StageRecord::IssueReservation {
                request,
                record,
                digest,
            })
        }
        SourceKind::Issue => {
            let legacy = crate::app_storage::decode::<Issue>(body)?;
            let expected = relative
                .split('/')
                .next()
                .ok_or(crate::Error::Config("legacy issue path is invalid"))?;
            if legacy.number != parse_number(expected)? {
                return Err(crate::Error::Config("legacy issue path and number differ"));
            }
            let request = request_id(&legacy.request_id, &legacy.request_id)?;
            Ok(StageRecord::Issue {
                request,
                record: issue_record(legacy)?,
            })
        }
        SourceKind::CommentSequence { issue } => Ok(StageRecord::CommentSequence {
            issue,
            last: crate::app_storage::decode_sequence(body)?,
        }),
        SourceKind::CommentReservation { issue } => {
            let legacy = crate::app_storage::decode::<Comment>(body)?;
            let request = request_id(&legacy.request_id, request_from_relative(relative)?)?;
            let record = comment_record(issue, legacy)?;
            if record.version != 1 || record.updated_at_ms != record.created_at_ms {
                return Err(crate::Error::Config(
                    "legacy comment reservation is not an initial proposal",
                ));
            }
            let digest = *comment_submission_digest(&CreateCommentInput {
                submission_id: request,
                issue,
                author: record.author.clone(),
                body: record.body.clone(),
            })
            .as_bytes();
            Ok(StageRecord::CommentReservation {
                request,
                record,
                digest,
            })
        }
        SourceKind::Comment { issue, number } => {
            let legacy = crate::app_storage::decode::<Comment>(body)?;
            if legacy.number != number {
                return Err(crate::Error::Config(
                    "legacy comment path and number differ",
                ));
            }
            let request = request_id(&legacy.request_id, &legacy.request_id)?;
            Ok(StageRecord::Comment {
                request,
                record: comment_record(issue, legacy)?,
            })
        }
    }
}

fn issue_record(legacy: Issue) -> crate::Result<IssueRecord> {
    let record = IssueRecord {
        number: legacy.number,
        author: author(legacy.author),
        title: legacy.title,
        body: legacy.body,
        state: match legacy.state {
            IssueState::Open => 0,
            IssueState::Closed => 1,
        },
        label_ids: legacy.label_ids,
        assignee_subjects: legacy.assignee_subjects,
        version: legacy.version,
        created_at_ms: legacy.created_at,
        updated_at_ms: legacy.updated_at,
    };
    validate_issue_v1(&record)?;
    Ok(record)
}

fn comment_record(issue: u64, legacy: Comment) -> crate::Result<CommentRecord> {
    let record = CommentRecord {
        issue,
        number: legacy.number,
        author: author(legacy.author),
        body: legacy.body,
        version: legacy.version,
        created_at_ms: legacy.created_at,
        updated_at_ms: legacy.updated_at,
    };
    validate_comment_v1(&record)?;
    Ok(record)
}

fn author(identity: crate::auth::Identity) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: identity.issuer,
        subject: identity.subject,
        name: identity.name,
    }
}

fn request_from_relative(relative: &str) -> crate::Result<&str> {
    relative
        .rsplit_once('/')
        .map(|(_, request)| request)
        .and_then(|request| request.strip_suffix(".json"))
        .ok_or(crate::Error::Config(
            "legacy issue reservation path is invalid",
        ))
}

fn request_id(value: &str, expected: &str) -> crate::Result<[u8; 16]> {
    let parsed = Uuid::parse_str(value)
        .map_err(|_| crate::Error::Config("legacy submission ID is not a UUID"))?;
    let canonical = parsed.hyphenated().to_string();
    if value != canonical || value != expected {
        return Err(crate::Error::Config(
            "legacy submission ID is not canonical or differs from its path",
        ));
    }
    Ok(parsed.into_bytes())
}

fn parse_request_filename(value: &str) -> crate::Result<[u8; 16]> {
    let request = value.strip_suffix(".json").ok_or(crate::Error::Config(
        "legacy issue reservation filename is invalid",
    ))?;
    request_id(request, request)
}

fn parse_number_filename(value: &str) -> crate::Result<u64> {
    let value = value
        .strip_suffix(".json")
        .ok_or(crate::Error::Config("legacy comment filename is invalid"))?;
    parse_number(value)
}

fn parse_number(value: &str) -> crate::Result<u64> {
    let number = value
        .parse::<u64>()
        .map_err(|_| crate::Error::Config("legacy issue path number is invalid"))?;
    if number == 0 || number > crate::app_storage::MAX_NUMBER || format!("{number:016}") != value {
        return Err(crate::Error::Config(
            "legacy issue path number is not canonical",
        ));
    }
    Ok(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_paths_are_exact_and_canonical() {
        assert!(matches!(
            classify("0000000000000001/comments/0000000000000002.json").unwrap(),
            SourceKind::Comment {
                issue: 1,
                number: 2
            }
        ));
        for invalid in [
            "1/issue.json",
            "0000000000000000/issue.json",
            "0000000000000001/comments/2.json",
            "0000000000000001/comments/unknown.json",
            "labels/sequence.json",
        ] {
            assert!(classify(invalid).is_err(), "{invalid}");
        }
    }
}
