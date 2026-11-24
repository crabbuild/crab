use super::{storage::result_optional_u64, validation::validate_oid, *};

#[derive(Deserialize, Serialize)]
pub(crate) struct ReviewNavigationInput {
    pub pull: u64,
    pub base_oid: String,
    pub head_oid: String,
    pub around: Option<u64>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct ReviewNavigation {
    pub open: u64,
    pub resolved: u64,
    pub outdated: u64,
    pub outdated_unresolved: u64,
    pub previous: Option<u64>,
    pub next: Option<u64>,
}

pub(crate) struct GetReviewNavigation;

impl Read for GetReviewNavigation {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 443;
    type Input = ReviewNavigationInput;
    type Output = Option<ReviewNavigation>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_number(input.pull)?;
        validate_oid(&input.base_oid)?;
        validate_oid(&input.head_oid)?;
        if let Some(around) = input.around {
            validate_number(around)?;
        }
        if !pull_exists_query(context, input.pull)? {
            return Ok(None);
        }
        // Thread numbers are immutable. Seek around a resolved or outdated
        // selection without requiring it to remain in the current filter.
        let results = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT
                    COUNT(CASE WHEN current AND resolved = 0 THEN 1 END),
                    COUNT(CASE WHEN current AND resolved = 1 THEN 1 END),
                    COUNT(CASE WHEN NOT current THEN 1 END),
                    COUNT(CASE WHEN NOT current AND resolved = 0 THEN 1 END),
                    MIN(CASE WHEN resolved = 0 AND number > ?4 THEN number END),
                    MAX(CASE WHEN resolved = 0 AND number < ?4 THEN number END)
                 FROM (
                    SELECT number, resolved, base_oid = ?1 AND head_oid = ?2 AS current
                    FROM repository_pull_review_threads WHERE pull_number = ?3
                 )",
                vec![
                    SqlValue::Text(input.base_oid),
                    SqlValue::Text(input.head_oid),
                    integer(input.pull)?,
                    integer(input.around.unwrap_or(MAX_NUMBER + 1))?,
                ],
            )],
        })?;
        let row = results[0].rows.first().ok_or(crate::cells::Error::Invalid(
            "repository review navigation query returned no summary",
        ))?;
        Ok(Some(ReviewNavigation {
            open: result_u64_from_row(row, 0)?,
            resolved: result_u64_from_row(row, 1)?,
            outdated: result_u64_from_row(row, 2)?,
            outdated_unresolved: result_u64_from_row(row, 3)?,
            previous: result_optional_u64(row, 4)?,
            next: result_optional_u64(row, 5)?,
        }))
    }
}
