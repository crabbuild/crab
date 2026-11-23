use super::*;
use crate::cells::repository::{
    MAX_NUMBER, SqlValue, integer, result_u64_from_row, search::index::rowid,
};

const MAX_REFERENCES: usize = 32;

pub(super) fn numbers(body: &str) -> Vec<u64> {
    if !body.contains('#') {
        return Vec::new();
    }
    let mut numbers = Vec::new();
    for range in crate::markdown::prose_ranges(body) {
        let prose = &body[range.clone()];
        for (offset, _) in prose.match_indices('#') {
            let before = &prose[..offset];
            let word = before.trim_end_matches([' ', '\t']);
            if word.len() == before.len() {
                continue;
            }
            let word = word.strip_suffix(':').unwrap_or(word);
            let start = word
                .trim_end_matches(|ch: char| ch.is_ascii_alphabetic())
                .len();
            let keyword = &word[start..];
            if ![
                "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves",
                "resolved",
            ]
            .iter()
            .any(|form| keyword.eq_ignore_ascii_case(form))
                || body[..range.start + start]
                    .chars()
                    .next_back()
                    .is_some_and(|ch| ch.is_alphanumeric() || "_./\\@-".contains(ch))
            {
                continue;
            }
            let tail = &prose[offset + 1..];
            let end = tail.bytes().take_while(u8::is_ascii_digit).count();
            if body[range.start + offset + 1 + end..]
                .chars()
                .next()
                .is_some_and(|ch| ch.is_alphanumeric() || "_/@-".contains(ch))
            {
                continue;
            }
            if let Ok(number) = tail[..end].parse::<u64>()
                && (1..=MAX_NUMBER).contains(&number)
                && !numbers.contains(&number)
            {
                numbers.push(number);
                if numbers.len() == MAX_REFERENCES {
                    return numbers;
                }
            }
        }
    }
    numbers
}

pub(super) fn page(context: &Transaction<'_>, input: &Query) -> crate::cells::Result<Vec<u64>> {
    let limit = SqlValue::Integer(i64::from(input.limit) + 1);
    let default = SqlValue::Text(input.default_ref.clone());
    let bound = |value: Option<u64>| value.map_or(Ok(SqlValue::Integer(i64::MAX)), integer);
    let rows = match input.subject.kind {
        SubjectKind::Pull => {
            context
                .query(
                    "SELECT k.issue FROM discussion_index_closing_issues k \
             JOIN repository_issues i ON i.number = k.issue \
             WHERE k.document = ? AND k.issue < ? AND (k.base_ref = ? OR k.base_ref IS NULL) \
             ORDER BY k.issue DESC LIMIT ?",
                    &[
                        integer(rowid(input.subject))?,
                        bound(input.before)?,
                        default,
                        limit,
                    ],
                )?
                .rows
        }
        SubjectKind::Issue => {
            let before = bound(input.before.map(|number| {
                rowid(Subject {
                    kind: SubjectKind::Pull,
                    number,
                })
            }))?;
            let issue = integer(input.subject.number)?;
            let mut rows = Vec::new();
            // Read the current branch and historical links through separate
            // index prefixes so a popular issue never scans unrelated branches.
            for branch in [default, SqlValue::Null] {
                rows.extend(
                    context
                        .query(
                            "SELECT d.number FROM discussion_index_closing_issues k \
                     JOIN discussion_index d ON d.rowid = k.document \
                     WHERE k.issue = ? AND k.base_ref IS ? AND k.document < ? \
                     ORDER BY k.document DESC LIMIT ?",
                            &[issue.clone(), branch, before.clone(), limit.clone()],
                        )?
                        .rows,
                );
            }
            rows
        }
    };
    rows.iter().map(|row| result_u64_from_row(row, 0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_references_are_literal_markdown_prose_and_exact_issue_numbers() {
        let body = "Fixes #3, **CLOSES: #2**. Resolves #3. Fixed\t#4 and resolved #5.\n\n\
                    `Closes #100` [Fixes #101](https://example.test) \\Fixes #102 Fixes \\#103 \
                    Fixes &#35;104 not-Fixes #105 Fixes #106oops Fixes #107/other \
                    Fixes elsewhere/project#108 Fixes #0 Fixes #9007199254740992.\n\n\
                    > Resolves #109\n\n```md\nCloses #110\n```\n\n<!-- Fixes #111 -->";
        assert_eq!(numbers(body), vec![3, 2, 4, 5]);
    }
}
