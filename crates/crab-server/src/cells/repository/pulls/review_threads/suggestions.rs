use super::*;

pub(crate) const MAX_SUGGESTIONS: usize = 20;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SuggestionVersion {
    pub number: u64,
    pub version: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SuggestionSelection {
    pub pull: u64,
    pub threads: Vec<SuggestionVersion>,
}

impl SuggestionSelection {
    fn validate(&self) -> crate::cells::Result<()> {
        validate_number(self.pull)?;
        if !(1..=MAX_SUGGESTIONS).contains(&self.threads.len())
            || self
                .threads
                .windows(2)
                .any(|pair| pair[0].number >= pair[1].number)
        {
            return Err(crate::cells::Error::Invalid("Invalid suggestion selection"));
        }
        for thread in &self.threads {
            validate_number(thread.number)?;
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrepareSuggestionsInput {
    pub selection: SuggestionSelection,
    pub base_oid: String,
    pub head_oid: String,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct PreparedSuggestion {
    pub author: RepositoryAuthor,
    pub path: Vec<u8>,
    pub blob: String,
    pub start_line: u64,
    pub end_line: u64,
    #[serde(with = "compact_text")]
    pub text: String,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum PrepareSuggestionsOutcome {
    Ready(Vec<PreparedSuggestion>),
    Missing,
    NotSuggestion,
    Changed,
}

pub(crate) struct PrepareSuggestions;

impl Read for PrepareSuggestions {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 444;
    type Input = PrepareSuggestionsInput;
    type Output = PrepareSuggestionsOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        use PrepareSuggestionsOutcome as Outcome;
        input.selection.validate()?;
        validation::validate_oid(&input.base_oid)?;
        validation::validate_oid(&input.head_oid)?;
        let mut suggestions = Vec::with_capacity(input.selection.threads.len());
        for selected in input.selection.threads {
            let rows = context.query(
                "SELECT version, resolved, suggested_text, new_blob_oid, base_oid, head_oid, side, author_issuer, author_subject, author_name, path, start_line, end_line
                 FROM repository_pull_review_threads WHERE pull_number = ? AND number = ?",
                &[integer(input.selection.pull)?, integer(selected.number)?],
            )?.rows;
            let Some(row) = rows.first() else {
                return Ok(Outcome::Missing);
            };
            let (Some(text), Some(blob)) =
                (result_optional_text(row, 2)?, result_optional_text(row, 3)?)
            else {
                return Ok(Outcome::NotSuggestion);
            };
            if !version_matches(row, &selected)?
                || result_text(row, 4)? != input.base_oid
                || result_text(row, 5)? != input.head_oid
                || PullReviewThreadSide::from_code(result_u64_from_row(row, 6)?)?
                    != PullReviewThreadSide::New
            {
                return Ok(Outcome::Changed);
            }
            validate_suggested_text(Some(&text))?;
            let author = storage::author_from_row(row, 7)?;
            validate_author(&author)?;
            suggestions.push(PreparedSuggestion {
                author,
                path: result_blob(row, 10)?.to_vec(),
                blob,
                start_line: result_u64_from_row(row, 11)?,
                end_line: result_u64_from_row(row, 12)?,
                text,
            });
        }
        Ok(Outcome::Ready(suggestions))
    }
}

pub(crate) struct VerifySuggestionVersions;

impl Read for VerifySuggestionVersions {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 445;
    type Input = SuggestionSelection;
    type Output = bool;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> crate::cells::Result<bool> {
        input.validate()?;
        for selected in input.threads {
            let rows = context.query(
                "SELECT version, resolved FROM repository_pull_review_threads WHERE pull_number = ? AND number = ?",
                &[integer(input.pull)?, integer(selected.number)?],
            )?.rows;
            let Some(row) = rows.first() else {
                return Ok(false);
            };
            if !version_matches(row, &selected)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn version_matches(row: &[SqlValue], selected: &SuggestionVersion) -> crate::cells::Result<bool> {
    Ok(result_u64_from_row(row, 0)? == selected.version && result_u64_from_row(row, 1)? == 0)
}

// Base64 bounds the JSON size even when every suggestion contains characters
// that JSON would escape. Twenty maximum-size texts and their metadata fit.
mod compact_text {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(text: &str, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(text))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let bytes = STANDARD.decode(&encoded).map_err(de::Error::custom)?;
        String::from_utf8(bytes).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximum_suggestion_batch_fits_the_cell_reply() {
        let text = "\u{0001}".repeat(MAX_SUGGESTED_TEXT_BYTES);
        let suggestions = (0..MAX_SUGGESTIONS)
            .map(|_| PreparedSuggestion {
                author: RepositoryAuthor {
                    issuer: "𐐀".repeat(512),
                    subject: "𐐁".repeat(512),
                    name: "𐐂".repeat(160),
                },
                path: vec![255; 1024],
                blob: "a".repeat(40),
                start_line: MAX_NUMBER,
                end_line: MAX_NUMBER,
                text: text.clone(),
            })
            .collect();
        let encoded = serde_json::to_vec(&PrepareSuggestionsOutcome::Ready(suggestions)).unwrap();
        assert!(encoded.len() + 256 < crate::cells::application::MAX_RESULT_BYTES);
        let decoded: PrepareSuggestionsOutcome = serde_json::from_slice(&encoded).unwrap();
        let PrepareSuggestionsOutcome::Ready(decoded) = decoded else {
            panic!("expected a prepared batch");
        };
        assert_eq!(decoded.len(), MAX_SUGGESTIONS);
        assert!(decoded.iter().all(|suggestion| suggestion.text == text));
    }
}
