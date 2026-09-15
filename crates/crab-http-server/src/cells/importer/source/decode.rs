use bytes::Bytes;
use uuid::Uuid;

use super::super::legacy::{
    Comment, CommitStatus, DeletedLabel, Issue, IssueState, Label, LabelCatalog, LabelReservation,
    StatusSummary,
};
use crate::cells::repository::{
    CommentRecord, CommitStatusRecord, CreateCommentInput, CreateCommitStatusInput,
    CreateIssueInput, CreateLabelInput, IssueRecord, LabelRecord, RepositoryAuthor,
    comment_submission_digest, issue_submission_digest, label_submission_digest,
    status_submission_digest,
};

use super::{
    labels, validate_author_v1, validate_comment_v1, validate_issue_v1, validate_status_v1,
};

#[derive(Clone)]
pub(super) enum SourceKind {
    IssueSequence,
    IssueReservation,
    Issue,
    CommentSequence { issue: u64 },
    CommentReservation { issue: u64 },
    Comment { issue: u64, number: u64 },
    LabelSequence,
    LabelReservation,
    LabelCatalog,
    StatusSequence { oid: String },
    StatusReservation { oid: String },
    StatusSummary { oid: String },
}

impl SourceKind {
    pub(super) const fn name(&self) -> &'static str {
        match self {
            Self::IssueSequence => "issue_sequence",
            Self::IssueReservation => "issue_reservation",
            Self::Issue => "issue",
            Self::CommentSequence { .. } => "comment_sequence",
            Self::CommentReservation { .. } => "comment_reservation",
            Self::Comment { .. } => "comment",
            Self::LabelSequence => "label_sequence",
            Self::LabelReservation => "label_reservation",
            Self::LabelCatalog => "label_catalog",
            Self::StatusSequence { .. } => "status_sequence",
            Self::StatusReservation { .. } => "status_reservation",
            Self::StatusSummary { .. } => "status_summary",
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
    LabelSequence(u64),
    LabelReservation {
        request: [u8; 16],
        record: LabelRecord,
        author: RepositoryAuthor,
        digest: [u8; 32],
    },
    LabelCatalog {
        labels: Vec<LabelRecord>,
        deleted: Vec<DeletedLabel>,
    },
    StatusSequence {
        oid: String,
        last: u64,
    },
    StatusReservation {
        record: CommitStatusRecord,
        digest: [u8; 32],
    },
    StatusSummary {
        oid: String,
        statuses: Vec<CommitStatusRecord>,
    },
}

pub(super) fn classify(relative: &str) -> crate::Result<SourceKind> {
    let parts = relative.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        ["issues", "sequence.json"] => Ok(SourceKind::IssueSequence),
        ["issues", "requests", request] => {
            parse_request_filename(request)?;
            Ok(SourceKind::IssueReservation)
        }
        ["issues", issue, "issue.json"] => {
            parse_number(issue)?;
            Ok(SourceKind::Issue)
        }
        ["issues", issue, "comments", "sequence.json"] => Ok(SourceKind::CommentSequence {
            issue: parse_number(issue)?,
        }),
        ["issues", issue, "comments", "requests", request] => {
            parse_request_filename(request)?;
            Ok(SourceKind::CommentReservation {
                issue: parse_number(issue)?,
            })
        }
        ["issues", issue, "comments", comment] => Ok(SourceKind::Comment {
            issue: parse_number(issue)?,
            number: parse_number_filename(comment)?,
        }),
        ["labels", "sequence.json"] => Ok(SourceKind::LabelSequence),
        ["labels", "requests", request] => {
            parse_request_filename(request)?;
            Ok(SourceKind::LabelReservation)
        }
        ["labels", "catalog.json"] => Ok(SourceKind::LabelCatalog),
        ["statuses", oid, "sequence.json"] => Ok(SourceKind::StatusSequence {
            oid: parse_oid(oid)?,
        }),
        ["statuses", oid, "requests", request] => {
            parse_request_filename(request)?;
            Ok(SourceKind::StatusReservation {
                oid: parse_oid(oid)?,
            })
        }
        ["statuses", oid, "summary.json"] => Ok(SourceKind::StatusSummary {
            oid: parse_oid(oid)?,
        }),
        _ => Err(crate::Error::Config(
            "legacy repository source contains an unknown object",
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
                .nth(1)
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
        SourceKind::LabelSequence => Ok(StageRecord::LabelSequence(
            crate::app_storage::decode_sequence(body)?,
        )),
        SourceKind::LabelReservation => {
            let legacy = crate::app_storage::decode::<LabelReservation>(body)?;
            let request = request_id(&legacy.request_id, request_from_relative(relative)?)?;
            let author = author(legacy.author);
            validate_author_v1(&author)?;
            let record = label_record(legacy.label)?;
            if record.version != 1 || record.updated_at_ms != record.created_at_ms {
                return Err(crate::Error::Config(
                    "legacy label reservation is not an initial proposal",
                ));
            }
            let digest = *label_submission_digest(&CreateLabelInput {
                submission_id: request,
                author: author.clone(),
                name: record.name.clone(),
                color: record.color.clone(),
                description: record.description.clone(),
            })
            .as_bytes();
            Ok(StageRecord::LabelReservation {
                request,
                record,
                author,
                digest,
            })
        }
        SourceKind::LabelCatalog => {
            let legacy = crate::app_storage::decode::<LabelCatalog>(body)?;
            let labels = legacy
                .labels
                .into_iter()
                .map(label_record)
                .collect::<crate::Result<Vec<_>>>()?;
            for deleted in &legacy.deleted {
                validate_label_number(deleted.number)?;
                validate_version(deleted.version)?;
            }
            Ok(StageRecord::LabelCatalog {
                labels,
                deleted: legacy.deleted,
            })
        }
        SourceKind::StatusSequence { oid } => Ok(StageRecord::StatusSequence {
            oid,
            last: crate::app_storage::decode_sequence(body)?,
        }),
        SourceKind::StatusReservation { oid } => {
            let legacy = crate::app_storage::decode::<CommitStatus>(body)?;
            let expected = request_from_relative(relative)?;
            let record = status_record(legacy, &oid, expected)?;
            let digest = *status_submission_digest(&CreateCommitStatusInput {
                submission_id: record.submission_id,
                author: record.author.clone(),
                oid: record.oid.clone(),
                context: record.context.clone(),
                state: record.state,
                description: record.description.clone(),
                target_url: record.target_url.clone(),
            })
            .as_bytes();
            Ok(StageRecord::StatusReservation { record, digest })
        }
        SourceKind::StatusSummary { oid } => {
            let legacy = crate::app_storage::decode::<StatusSummary>(body)?;
            if legacy.oid != oid {
                return Err(crate::Error::Config(
                    "legacy status summary path and commit differ",
                ));
            }
            let statuses = legacy
                .statuses
                .into_iter()
                .map(|status| {
                    let request = status.request_id.clone();
                    status_record(status, &oid, &request)
                })
                .collect::<crate::Result<Vec<_>>>()?;
            Ok(StageRecord::StatusSummary { oid, statuses })
        }
    }
}

fn status_record(
    legacy: CommitStatus,
    oid: &str,
    expected_request: &str,
) -> crate::Result<CommitStatusRecord> {
    if legacy.oid != oid {
        return Err(crate::Error::Config("legacy status path and commit differ"));
    }
    let record = CommitStatusRecord {
        number: legacy.number,
        submission_id: request_id(&legacy.request_id, expected_request)?,
        author: author(legacy.author),
        oid: legacy.oid,
        context: legacy.context,
        state: match legacy.state {
            crate::statuses::StatusState::Error => 0,
            crate::statuses::StatusState::Failure => 1,
            crate::statuses::StatusState::Pending => 2,
            crate::statuses::StatusState::Success => 3,
        },
        description: legacy.description,
        target_url: legacy.target_url,
        created_at_ms: legacy.created_at,
    };
    validate_status_v1(&record)?;
    Ok(record)
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

fn label_record(legacy: Label) -> crate::Result<LabelRecord> {
    let record = LabelRecord {
        number: legacy.number,
        name: legacy.name,
        color: legacy.color,
        description: legacy.description,
        version: legacy.version,
        created_at_ms: legacy.created_at,
        updated_at_ms: legacy.updated_at,
    };
    labels::validate_v1(&record)?;
    Ok(record)
}

fn validate_label_number(number: u64) -> crate::Result<()> {
    if number == 0 || number > 500 {
        return Err(crate::Error::Config(
            "legacy label number violates repository schema v1",
        ));
    }
    Ok(())
}

fn validate_version(version: u64) -> crate::Result<()> {
    if version == 0 || version > crate::app_storage::MAX_NUMBER {
        return Err(crate::Error::Config(
            "legacy label version violates repository schema v1",
        ));
    }
    Ok(())
}

fn parse_oid(value: &str) -> crate::Result<String> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || value.bytes().all(|byte| byte == b'0')
    {
        return Err(crate::Error::Config("legacy status commit ID is invalid"));
    }
    Ok(value.to_owned())
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
            "legacy repository reservation path is invalid",
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
        "legacy repository reservation filename is invalid",
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
            classify("issues/0000000000000001/comments/0000000000000002.json").unwrap(),
            SourceKind::Comment {
                issue: 1,
                number: 2
            }
        ));
        assert!(matches!(
            classify("labels/catalog.json").unwrap(),
            SourceKind::LabelCatalog
        ));
        assert!(matches!(
            classify("statuses/0123456789abcdef0123456789abcdef01234567/summary.json").unwrap(),
            SourceKind::StatusSummary { .. }
        ));
        for invalid in [
            "issues/1/issue.json",
            "issues/0000000000000000/issue.json",
            "issues/0000000000000001/comments/2.json",
            "issues/0000000000000001/comments/unknown.json",
            "labels/unknown.json",
            "statuses/0123456789ABCDEF0123456789ABCDEF01234567/summary.json",
            "statuses/0000000000000000000000000000000000000000/summary.json",
            "statuses/01234567/sequence.json",
        ] {
            assert!(classify(invalid).is_err(), "{invalid}");
        }
    }
}
