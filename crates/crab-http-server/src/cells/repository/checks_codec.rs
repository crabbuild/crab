use crab_cell_runtime::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};

use super::{
    CheckAnnotationRecord, CheckOutputRecord, CheckReportInput, CheckRunDetail, CheckRunKey,
    CheckRunPage, CheckRunRecord, CheckStepRecord, CheckSubmissionKey, CreateCheckRunInput,
    CreateCheckRunOutcome, ListCheckRunsInput, RepositoryAuthor, UpdateCheckRunInput,
    UpdateCheckRunOutcome,
};

impl WireValue for CheckStepRecord {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_text(&self.name)?;
        encoder.write_u8(self.status)?;
        self.conclusion.encode(encoder)?;
        self.log.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            name: decoder.read_text()?.to_owned(),
            status: decoder.read_u8()?,
            conclusion: Option::<u8>::decode(decoder)?,
            log: Option::<String>::decode(decoder)?,
        })
    }
}

impl WireValue for CheckAnnotationRecord {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_text(&self.path)?;
        encoder.write_u64(self.start_line)?;
        encoder.write_u64(self.end_line)?;
        encoder.write_u8(self.level)?;
        self.title.encode(encoder)?;
        encoder.write_text(&self.message)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            path: decoder.read_text()?.to_owned(),
            start_line: decoder.read_u64()?,
            end_line: decoder.read_u64()?,
            level: decoder.read_u8()?,
            title: Option::<String>::decode(decoder)?,
            message: decoder.read_text()?.to_owned(),
        })
    }
}

impl WireValue for CheckOutputRecord {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_text(&self.title)?;
        encoder.write_text(&self.summary)?;
        self.text.encode(encoder)?;
        encoder.write_count(self.steps.len())?;
        for step in &self.steps {
            step.encode(encoder)?;
        }
        encoder.write_count(self.annotations.len())?;
        for annotation in &self.annotations {
            annotation.encode(encoder)?;
        }
        Ok(())
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let title = decoder.read_text()?.to_owned();
        let summary = decoder.read_text()?.to_owned();
        let text = Option::<String>::decode(decoder)?;
        let step_count = bounded_count(decoder, 50)?;
        let mut steps = Vec::with_capacity(step_count);
        for _ in 0..step_count {
            steps.push(CheckStepRecord::decode(decoder)?);
        }
        let annotation_count = bounded_count(decoder, 50)?;
        let mut annotations = Vec::with_capacity(annotation_count);
        for _ in 0..annotation_count {
            annotations.push(CheckAnnotationRecord::decode(decoder)?);
        }
        Ok(Self {
            title,
            summary,
            text,
            steps,
            annotations,
        })
    }
}

impl WireValue for CheckReportInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u8(self.status)?;
        self.conclusion.encode(encoder)?;
        self.details_url.encode(encoder)?;
        self.output.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            status: decoder.read_u8()?,
            conclusion: Option::<u8>::decode(decoder)?,
            details_url: Option::<String>::decode(decoder)?,
            output: CheckOutputRecord::decode(decoder)?,
        })
    }
}

impl WireValue for CheckRunRecord {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.number)?;
        encoder.write_bytes(&self.create_submission_id)?;
        self.author.encode(encoder)?;
        encoder.write_text(&self.oid)?;
        encoder.write_text(&self.name)?;
        encoder.write_u8(self.status)?;
        self.conclusion.encode(encoder)?;
        self.details_url.encode(encoder)?;
        encoder.write_text(&self.output_title)?;
        encoder.write_u64(self.version)?;
        self.started_at_ms.encode(encoder)?;
        self.completed_at_ms.encode(encoder)?;
        encoder.write_u64(self.created_at_ms)?;
        encoder.write_u64(self.updated_at_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            number: decoder.read_u64()?,
            create_submission_id: read_fixed(decoder, "check submission ID length")?,
            author: RepositoryAuthor::decode(decoder)?,
            oid: decoder.read_text()?.to_owned(),
            name: decoder.read_text()?.to_owned(),
            status: decoder.read_u8()?,
            conclusion: Option::<u8>::decode(decoder)?,
            details_url: Option::<String>::decode(decoder)?,
            output_title: decoder.read_text()?.to_owned(),
            version: decoder.read_u64()?,
            started_at_ms: Option::<u64>::decode(decoder)?,
            completed_at_ms: Option::<u64>::decode(decoder)?,
            created_at_ms: decoder.read_u64()?,
            updated_at_ms: decoder.read_u64()?,
        })
    }
}

impl WireValue for CheckRunDetail {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        self.run.encode(encoder)?;
        self.output.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            run: CheckRunRecord::decode(decoder)?,
            output: CheckOutputRecord::decode(decoder)?,
        })
    }
}

impl WireValue for CreateCheckRunInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.submission_id)?;
        self.author.encode(encoder)?;
        encoder.write_text(&self.oid)?;
        encoder.write_text(&self.name)?;
        self.report.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            submission_id: read_fixed(decoder, "check submission ID length")?,
            author: RepositoryAuthor::decode(decoder)?,
            oid: decoder.read_text()?.to_owned(),
            name: decoder.read_text()?.to_owned(),
            report: CheckReportInput::decode(decoder)?,
        })
    }
}

impl WireValue for CreateCheckRunOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Created(detail) => {
                encoder.write_u8(1)?;
                detail.encode(encoder)
            }
            Self::RequestConflict => encoder.write_u8(2),
            Self::RunLimit => encoder.write_u8(3),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            1 => Ok(Self::Created(Box::new(CheckRunDetail::decode(decoder)?))),
            2 => Ok(Self::RequestConflict),
            3 => Ok(Self::RunLimit),
            _ => Err(CodecError::Invalid("invalid create-check outcome")),
        }
    }
}

impl WireValue for UpdateCheckRunInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.submission_id)?;
        self.actor.encode(encoder)?;
        encoder.write_text(&self.oid)?;
        encoder.write_u64(self.number)?;
        encoder.write_u64(self.version)?;
        self.report.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            submission_id: read_fixed(decoder, "check submission ID length")?,
            actor: RepositoryAuthor::decode(decoder)?,
            oid: decoder.read_text()?.to_owned(),
            number: decoder.read_u64()?,
            version: decoder.read_u64()?,
            report: CheckReportInput::decode(decoder)?,
        })
    }
}

impl WireValue for UpdateCheckRunOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Updated(detail) => {
                encoder.write_u8(1)?;
                detail.encode(encoder)
            }
            Self::RequestConflict => encoder.write_u8(2),
            Self::NotFound => encoder.write_u8(3),
            Self::Forbidden => encoder.write_u8(4),
            Self::Conflict => encoder.write_u8(5),
            Self::InvalidTransition => encoder.write_u8(6),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            1 => Ok(Self::Updated(Box::new(CheckRunDetail::decode(decoder)?))),
            2 => Ok(Self::RequestConflict),
            3 => Ok(Self::NotFound),
            4 => Ok(Self::Forbidden),
            5 => Ok(Self::Conflict),
            6 => Ok(Self::InvalidTransition),
            _ => Err(CodecError::Invalid("invalid update-check outcome")),
        }
    }
}

impl WireValue for CheckRunKey {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_text(&self.oid)?;
        encoder.write_u64(self.number)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            oid: decoder.read_text()?.to_owned(),
            number: decoder.read_u64()?,
        })
    }
}

impl WireValue for CheckSubmissionKey {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.submission_id)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            submission_id: read_fixed(decoder, "check submission ID length")?,
        })
    }
}

impl WireValue for ListCheckRunsInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_text(&self.oid)?;
        self.before.encode(encoder)?;
        encoder.write_u8(self.limit)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            oid: decoder.read_text()?.to_owned(),
            before: Option::<u64>::decode(decoder)?,
            limit: decoder.read_u8()?,
        })
    }
}

impl WireValue for CheckRunPage {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_count(self.runs.len())?;
        for run in &self.runs {
            run.encode(encoder)?;
        }
        self.next.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let count = bounded_count(decoder, 100)?;
        let mut runs = Vec::with_capacity(count);
        for _ in 0..count {
            runs.push(CheckRunRecord::decode(decoder)?);
        }
        Ok(Self {
            runs,
            next: Option::<u64>::decode(decoder)?,
        })
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

fn bounded_count(decoder: &mut BoundedDecoder<'_>, maximum: usize) -> Result<usize, CodecError> {
    let count = decoder.read_count()?;
    if count > maximum {
        return Err(CodecError::Invalid("wire collection is too large"));
    }
    Ok(count)
}
