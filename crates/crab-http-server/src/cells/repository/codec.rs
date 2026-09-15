use crab_cell_runtime::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};

use super::{
    CommentKey, CommentPage, CommentRecord, CreateCommentInput, CreateCommentOutcome,
    CreateIssueInput, CreateIssueOutcome, IssuePage, IssueRecord, IssueSummary, ListCommentsInput,
    ListIssuesInput, RepositoryAuthor, UpdateCommentInput, UpdateCommentOutcome, UpdateIssueInput,
    UpdateIssueOutcome,
};

impl WireValue for RepositoryAuthor {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_text(&self.issuer)?;
        encoder.write_text(&self.subject)?;
        encoder.write_text(&self.name)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            issuer: decoder.read_text()?.to_owned(),
            subject: decoder.read_text()?.to_owned(),
            name: decoder.read_text()?.to_owned(),
        })
    }
}

impl WireValue for CreateIssueInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.submission_id)?;
        self.author.encode(encoder)?;
        encoder.write_text(&self.title)?;
        encoder.write_text(&self.body)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            submission_id: read_fixed(decoder, "issue submission ID length")?,
            author: RepositoryAuthor::decode(decoder)?,
            title: decoder.read_text()?.to_owned(),
            body: decoder.read_text()?.to_owned(),
        })
    }
}

impl WireValue for CreateIssueOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Created(record) => {
                encoder.write_u8(1)?;
                record.encode(encoder)
            }
            Self::RequestConflict => encoder.write_u8(2),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            1 => Ok(Self::Created(Box::new(IssueRecord::decode(decoder)?))),
            2 => Ok(Self::RequestConflict),
            _ => Err(CodecError::Invalid("invalid create-issue outcome")),
        }
    }
}

impl WireValue for IssueRecord {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.number)?;
        self.author.encode(encoder)?;
        encoder.write_text(&self.title)?;
        encoder.write_text(&self.body)?;
        encoder.write_u8(self.state)?;
        encode_u64s(&self.label_ids, encoder)?;
        encode_strings(&self.assignee_subjects, encoder)?;
        encoder.write_u64(self.version)?;
        encoder.write_u64(self.created_at_ms)?;
        encoder.write_u64(self.updated_at_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            number: decoder.read_u64()?,
            author: RepositoryAuthor::decode(decoder)?,
            title: decoder.read_text()?.to_owned(),
            body: decoder.read_text()?.to_owned(),
            state: decoder.read_u8()?,
            label_ids: decode_u64s(decoder, 20)?,
            assignee_subjects: decode_strings(decoder, 10)?,
            version: decoder.read_u64()?,
            created_at_ms: decoder.read_u64()?,
            updated_at_ms: decoder.read_u64()?,
        })
    }
}

impl WireValue for CreateCommentInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.submission_id)?;
        encoder.write_u64(self.issue)?;
        self.author.encode(encoder)?;
        encoder.write_text(&self.body)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            submission_id: read_fixed(decoder, "comment submission ID length")?,
            issue: decoder.read_u64()?,
            author: RepositoryAuthor::decode(decoder)?,
            body: decoder.read_text()?.to_owned(),
        })
    }
}

impl WireValue for CommentRecord {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.issue)?;
        encoder.write_u64(self.number)?;
        self.author.encode(encoder)?;
        encoder.write_text(&self.body)?;
        encoder.write_u64(self.version)?;
        encoder.write_u64(self.created_at_ms)?;
        encoder.write_u64(self.updated_at_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            issue: decoder.read_u64()?,
            number: decoder.read_u64()?,
            author: RepositoryAuthor::decode(decoder)?,
            body: decoder.read_text()?.to_owned(),
            version: decoder.read_u64()?,
            created_at_ms: decoder.read_u64()?,
            updated_at_ms: decoder.read_u64()?,
        })
    }
}

impl WireValue for CreateCommentOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Created(record) => {
                encoder.write_u8(1)?;
                record.encode(encoder)
            }
            Self::IssueNotFound => encoder.write_u8(2),
            Self::RequestConflict => encoder.write_u8(3),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            1 => Ok(Self::Created(CommentRecord::decode(decoder)?)),
            2 => Ok(Self::IssueNotFound),
            3 => Ok(Self::RequestConflict),
            _ => Err(CodecError::Invalid("invalid create-comment outcome")),
        }
    }
}

fn read_fixed<const N: usize>(
    decoder: &mut BoundedDecoder<'_>,
    context: &'static str,
) -> Result<[u8; N], CodecError> {
    decoder
        .read_bytes()?
        .try_into()
        .map_err(|_| CodecError::Invalid(context))
}

impl WireValue for CommentKey {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.issue)?;
        encoder.write_u64(self.number)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            issue: decoder.read_u64()?,
            number: decoder.read_u64()?,
        })
    }
}

impl WireValue for UpdateIssueInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.number)?;
        self.actor.encode(encoder)?;
        encoder.write_bool(self.can_manage_metadata)?;
        encoder.write_u64(self.version)?;
        self.title.encode(encoder)?;
        self.body.encode(encoder)?;
        self.state.encode(encoder)?;
        encode_optional_u64s(self.label_ids.as_deref(), encoder)?;
        encode_optional_strings(self.assignee_subjects.as_deref(), encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            number: decoder.read_u64()?,
            actor: RepositoryAuthor::decode(decoder)?,
            can_manage_metadata: decoder.read_bool()?,
            version: decoder.read_u64()?,
            title: Option::<String>::decode(decoder)?,
            body: Option::<String>::decode(decoder)?,
            state: Option::<u8>::decode(decoder)?,
            label_ids: decode_optional_u64s(decoder, 20)?,
            assignee_subjects: decode_optional_strings(decoder, 10)?,
        })
    }
}

impl WireValue for UpdateIssueOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Updated(issue) => {
                encoder.write_u8(1)?;
                issue.encode(encoder)
            }
            Self::NotFound => encoder.write_u8(2),
            Self::Forbidden => encoder.write_u8(3),
            Self::LabelForbidden => encoder.write_u8(4),
            Self::AssigneeForbidden => encoder.write_u8(5),
            Self::Conflict => encoder.write_u8(6),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            1 => Ok(Self::Updated(Box::new(IssueRecord::decode(decoder)?))),
            2 => Ok(Self::NotFound),
            3 => Ok(Self::Forbidden),
            4 => Ok(Self::LabelForbidden),
            5 => Ok(Self::AssigneeForbidden),
            6 => Ok(Self::Conflict),
            _ => Err(CodecError::Invalid("invalid update-issue outcome")),
        }
    }
}

impl WireValue for UpdateCommentInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        self.key.encode(encoder)?;
        self.actor.encode(encoder)?;
        encoder.write_u64(self.version)?;
        encoder.write_text(&self.body)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            key: CommentKey::decode(decoder)?,
            actor: RepositoryAuthor::decode(decoder)?,
            version: decoder.read_u64()?,
            body: decoder.read_text()?.to_owned(),
        })
    }
}

impl WireValue for UpdateCommentOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Updated(comment) => {
                encoder.write_u8(1)?;
                comment.encode(encoder)
            }
            Self::NotFound => encoder.write_u8(2),
            Self::Forbidden => encoder.write_u8(3),
            Self::Conflict => encoder.write_u8(4),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            1 => Ok(Self::Updated(CommentRecord::decode(decoder)?)),
            2 => Ok(Self::NotFound),
            3 => Ok(Self::Forbidden),
            4 => Ok(Self::Conflict),
            _ => Err(CodecError::Invalid("invalid update-comment outcome")),
        }
    }
}

impl WireValue for ListIssuesInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        self.before.encode(encoder)?;
        encoder.write_u8(self.limit)?;
        encoder.write_u8(self.state)?;
        self.query.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            before: Option::<u64>::decode(decoder)?,
            limit: decoder.read_u8()?,
            state: decoder.read_u8()?,
            query: Option::<String>::decode(decoder)?,
        })
    }
}

impl WireValue for IssueSummary {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.number)?;
        self.author.encode(encoder)?;
        encoder.write_text(&self.title)?;
        encoder.write_u8(self.state)?;
        encode_u64s(&self.label_ids, encoder)?;
        encode_strings(&self.assignee_subjects, encoder)?;
        encoder.write_u64(self.version)?;
        encoder.write_u64(self.created_at_ms)?;
        encoder.write_u64(self.updated_at_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            number: decoder.read_u64()?,
            author: RepositoryAuthor::decode(decoder)?,
            title: decoder.read_text()?.to_owned(),
            state: decoder.read_u8()?,
            label_ids: decode_u64s(decoder, 20)?,
            assignee_subjects: decode_strings(decoder, 10)?,
            version: decoder.read_u64()?,
            created_at_ms: decoder.read_u64()?,
            updated_at_ms: decoder.read_u64()?,
        })
    }
}

impl WireValue for IssuePage {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_count(self.items.len())?;
        for issue in &self.items {
            issue.encode(encoder)?;
        }
        self.next.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let count = bounded_count(decoder, 50)?;
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(IssueSummary::decode(decoder)?);
        }
        Ok(Self {
            items,
            next: Option::<u64>::decode(decoder)?,
        })
    }
}

impl WireValue for ListCommentsInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.issue)?;
        self.before.encode(encoder)?;
        encoder.write_u8(self.limit)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            issue: decoder.read_u64()?,
            before: Option::<u64>::decode(decoder)?,
            limit: decoder.read_u8()?,
        })
    }
}

impl WireValue for CommentPage {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Found { items, next } => {
                encoder.write_u8(1)?;
                encoder.write_count(items.len())?;
                for comment in items {
                    comment.encode(encoder)?;
                }
                next.encode(encoder)
            }
            Self::IssueNotFound => encoder.write_u8(2),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            1 => {
                let count = bounded_count(decoder, 50)?;
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(CommentRecord::decode(decoder)?);
                }
                Ok(Self::Found {
                    items,
                    next: Option::<u64>::decode(decoder)?,
                })
            }
            2 => Ok(Self::IssueNotFound),
            _ => Err(CodecError::Invalid("invalid comment-page outcome")),
        }
    }
}

fn encode_u64s(values: &[u64], encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
    encoder.write_count(values.len())?;
    for value in values {
        encoder.write_u64(*value)?;
    }
    Ok(())
}

fn decode_u64s(decoder: &mut BoundedDecoder<'_>, maximum: usize) -> Result<Vec<u64>, CodecError> {
    let count = bounded_count(decoder, maximum)?;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(decoder.read_u64()?);
    }
    Ok(values)
}

fn encode_strings(values: &[String], encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
    encoder.write_count(values.len())?;
    for value in values {
        encoder.write_text(value)?;
    }
    Ok(())
}

fn decode_strings(
    decoder: &mut BoundedDecoder<'_>,
    maximum: usize,
) -> Result<Vec<String>, CodecError> {
    let count = bounded_count(decoder, maximum)?;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(decoder.read_text()?.to_owned());
    }
    Ok(values)
}

fn encode_optional_u64s(
    values: Option<&[u64]>,
    encoder: &mut BoundedEncoder,
) -> Result<(), CodecError> {
    match values {
        Some(values) => {
            encoder.write_u8(1)?;
            encode_u64s(values, encoder)
        }
        None => encoder.write_u8(0),
    }
}

fn decode_optional_u64s(
    decoder: &mut BoundedDecoder<'_>,
    maximum: usize,
) -> Result<Option<Vec<u64>>, CodecError> {
    match decoder.read_u8()? {
        0 => Ok(None),
        1 => Ok(Some(decode_u64s(decoder, maximum)?)),
        _ => Err(CodecError::Invalid("invalid optional list tag")),
    }
}

fn encode_optional_strings(
    values: Option<&[String]>,
    encoder: &mut BoundedEncoder,
) -> Result<(), CodecError> {
    match values {
        Some(values) => {
            encoder.write_u8(1)?;
            encode_strings(values, encoder)
        }
        None => encoder.write_u8(0),
    }
}

fn decode_optional_strings(
    decoder: &mut BoundedDecoder<'_>,
    maximum: usize,
) -> Result<Option<Vec<String>>, CodecError> {
    match decoder.read_u8()? {
        0 => Ok(None),
        1 => Ok(Some(decode_strings(decoder, maximum)?)),
        _ => Err(CodecError::Invalid("invalid optional list tag")),
    }
}

fn bounded_count(decoder: &mut BoundedDecoder<'_>, maximum: usize) -> Result<usize, CodecError> {
    let count = decoder.read_count()?;
    if count > maximum {
        return Err(CodecError::Invalid("wire collection is too large"));
    }
    Ok(count)
}
