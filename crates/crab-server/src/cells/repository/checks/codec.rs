use super::{CheckAnnotationRecord, CheckOutputRecord, CheckStepRecord};
use crate::cells::records::{RecordDecoder, RecordEncoder, RecordError, StoredRecord};

impl StoredRecord for CheckStepRecord {
    fn encode(&self, encoder: &mut RecordEncoder) -> Result<(), RecordError> {
        encoder.write_text(&self.name)?;
        encoder.write_u8(self.status)?;
        self.conclusion.encode(encoder)?;
        self.log.encode(encoder)
    }

    fn decode(decoder: &mut RecordDecoder<'_>) -> Result<Self, RecordError> {
        Ok(Self {
            name: decoder.read_text()?.to_owned(),
            status: decoder.read_u8()?,
            conclusion: Option::<u8>::decode(decoder)?,
            log: Option::<String>::decode(decoder)?,
        })
    }
}

impl StoredRecord for CheckAnnotationRecord {
    fn encode(&self, encoder: &mut RecordEncoder) -> Result<(), RecordError> {
        encoder.write_text(&self.path)?;
        encoder.write_u64(self.start_line)?;
        encoder.write_u64(self.end_line)?;
        encoder.write_u8(self.level)?;
        self.title.encode(encoder)?;
        encoder.write_text(&self.message)
    }

    fn decode(decoder: &mut RecordDecoder<'_>) -> Result<Self, RecordError> {
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

impl StoredRecord for CheckOutputRecord {
    fn encode(&self, encoder: &mut RecordEncoder) -> Result<(), RecordError> {
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

    fn decode(decoder: &mut RecordDecoder<'_>) -> Result<Self, RecordError> {
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

fn bounded_count(decoder: &mut RecordDecoder<'_>, maximum: usize) -> Result<usize, RecordError> {
    let count = decoder.read_count()?;
    if count > maximum {
        return Err(RecordError::Invalid("wire collection is too large"));
    }
    Ok(count)
}
