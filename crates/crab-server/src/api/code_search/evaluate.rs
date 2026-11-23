use super::{qualifiers::Predicate, *};
use crab_remote_git::TreeEntry;
use std::borrow::Cow;

impl Pattern {
    pub(super) fn repository_facts(&self, repo: &Repository) -> Facts {
        self.query.facts(|term| term.repository_fact(repo))
    }

    pub(super) fn file_facts(&self, path: &[u8], scope: &Facts) -> Facts {
        // Avoid allocating an escaped display path for ordinary UTF-8 names.
        let display = std::str::from_utf8(path)
            .ok()
            .filter(|name| !name.contains('%'))
            .map_or_else(|| Cow::Owned(display_path(path)), Cow::Borrowed);
        let mut repo = scope.iter().copied();
        self.query
            .facts(|term| term.file_fact(path, &display, repo.next().unwrap_or(Truth::Unknown)))
    }

    pub(super) fn index_facts(&self, row: &code_index::Row, facts: &mut [Truth]) {
        for (term, fact) in self.query.leaves.iter().zip(facts) {
            if *fact != Truth::Unknown {
                continue;
            }
            let Predicate::Text {
                kind,
                mask: Some(mask),
                ..
            } = term
            else {
                continue;
            };
            let filter = if *kind == Kind::Symbol {
                row.symbols.as_deref().or(row.filter.as_ref())
            } else {
                row.filter.as_ref()
            };
            // The filter proves absence only. A positive filter must still
            // reach Git, including when the term appears under NOT.
            if filter.is_some_and(|filter| !filter.may_contain(mask, self.case_sensitive)) {
                *fact = Truth::No;
            }
        }
    }

    pub(super) async fn inspect_entry(
        &self,
        snapshot: &RemoteGitSnapshot,
        entry: &TreeEntry,
        scope: &Facts,
        operation: &OperationContext,
    ) -> crab_remote_git::Result<(Inspection, bool)> {
        let path = entry.path.as_bytes();
        let facts = self.file_facts(path, scope);
        let matched = |lines, more_lines| {
            Inspection::Match(FileMatches {
                path: display_path(path),
                path_hex: encode_hex(path),
                lines,
                more_lines,
            })
        };
        match self.query.evaluate(&facts).0 {
            Truth::No => return Ok((Inspection::NoMatch, false)),
            Truth::Yes => return Ok((matched(Vec::new(), false), false)),
            Truth::Unknown => (),
        }
        let Some(text) = code_index::text(snapshot, &entry.path, entry.oid, operation).await?
        else {
            // An unreadable body supplies no evidence either way for NOT.
            return Ok((Inspection::Skipped, true));
        };
        let syntax = self.has_symbols.then(|| Syntax::for_path(path)).flatten();
        let cancel = operation.cancellation().clone();
        let query = self.query.clone();
        let result =
            tokio::task::spawn_blocking(move || verify(&query, facts, syntax, &text, &cancel))
                .await
                .map_err(|source| Error::DecodeTask { source })?;
        Ok((
            match result {
                Some((Truth::Yes, lines, more_lines)) => matched(lines, more_lines),
                Some((Truth::No, _, _)) => Inspection::NoMatch,
                _ => Inspection::Skipped,
            },
            true,
        ))
    }
}

fn verify(
    query: &QueryPlan,
    mut facts: Facts,
    syntax: Option<Syntax>,
    text: &str,
    cancel: &CancellationToken,
) -> Option<(Truth, Vec<Excerpt>, bool)> {
    let definitions =
        syntax.and_then(|syntax| code_index::symbols::definitions(syntax, text, cancel));
    let mut hits = Vec::with_capacity(query.leaves.len());
    for (term, fact) in query.leaves.iter().zip(&mut facts) {
        if cancel.is_cancelled() {
            return None;
        }
        let found = match (term, *fact) {
            (
                Predicate::Text {
                    kind: Kind::Symbol,
                    matcher,
                    ..
                },
                Truth::Unknown,
            ) => definitions
                .as_deref()
                .map(|definitions| symbol_matches(definitions, text, matcher)),
            (Predicate::Text { matcher, .. }, Truth::Unknown) => {
                expression_matches(text, matcher, cancel)
            }
            _ => None,
        };
        if let Some((lines, _)) = &found {
            *fact = Truth::from_bool(!lines.is_empty());
        }
        hits.push(found);
    }
    let (truth, evidence) = query.evaluate(&facts);
    if truth != Truth::Yes {
        return Some((truth, Vec::new(), false));
    }
    let mut more = false;
    let mut lines = Vec::new();
    for (index, hit) in hits.into_iter().enumerate() {
        if evidence & (1 << index) != 0
            && let Some((found, truncated)) = hit
        {
            more |= truncated;
            lines.extend(found);
        }
    }
    lines.sort_by_key(|line| line.line);
    lines.dedup_by_key(|line| line.line);
    more |= lines.len() > MAX_MATCH_LINES;
    lines.truncate(MAX_MATCH_LINES);
    Some((truth, lines, more))
}
