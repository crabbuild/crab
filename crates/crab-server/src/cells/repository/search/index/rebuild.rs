use super::*;

/// Start or advance preparation using its expected generation and version.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum RebuildSearchInput {
    Begin {
        expected: Option<Uuid>,
        generation: Uuid,
    },
    Step {
        generation: Uuid,
        version: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RebuildSearchOutcome {
    Progress { status: SearchIndexStatus },
    Conflict { status: SearchIndexStatus },
}

pub(crate) struct RebuildIndex;
impl Mutation for RebuildIndex {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 271;
    type Input = RebuildSearchInput;
    type Output = RebuildSearchOutcome;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        let current = state(context)?;
        let next = match input {
            RebuildSearchInput::Begin {
                expected,
                generation,
            } => {
                if current
                    .as_ref()
                    .is_some_and(|state| state.generation == generation)
                {
                    return progress(context);
                }
                if current.as_ref().map(|state| state.generation) != expected
                    || current
                        .as_ref()
                        .is_some_and(|state| state.progress.is_some())
                {
                    return conflict(context);
                }
                if generation.is_nil() {
                    return Err(crate::cells::Error::Invalid(
                        "discussion index generation is invalid",
                    ));
                }
                context.install_schema(include_str!("schema.sql"))?;
                State {
                    format: FORMAT,
                    generation,
                    progress: Some(SearchIndexProgress {
                        phase: SearchIndexPhase::Clear,
                        after: 0,
                        processed: 0,
                        version: 1,
                    }),
                }
            }
            RebuildSearchInput::Step {
                generation,
                version,
            } => {
                let Some(mut current) = current else {
                    return conflict(context);
                };
                if current.generation != generation {
                    return conflict(context);
                }
                let Some(build) = current.progress.as_mut() else {
                    return progress(context);
                };
                if version < build.version {
                    return progress(context);
                }
                if version != build.version {
                    return conflict(context);
                }
                let (sql, parameters) = match build.phase {
                    SearchIndexPhase::Clear => (
                        "SELECT rowid FROM discussion_index ORDER BY rowid LIMIT 8",
                        vec![],
                    ),
                    SearchIndexPhase::Issues => (
                        "SELECT number FROM repository_issues WHERE number > ? ORDER BY number LIMIT 8",
                        vec![integer(build.after)?],
                    ),
                    SearchIndexPhase::Pulls => (
                        "SELECT number FROM repository_pulls WHERE number > ? ORDER BY number LIMIT 8",
                        vec![integer(build.after)?],
                    ),
                };
                let rows = context.sql(&SqlBatch {
                    statements: vec![statement(sql, parameters)],
                })?;
                for row in &rows[0].rows {
                    let number = crate::cells::repository::result_u64_from_row(row, 0)?;
                    match build.phase {
                        SearchIndexPhase::Clear => projection::remove(context, number)?,
                        SearchIndexPhase::Issues | SearchIndexPhase::Pulls => {
                            let kind = if build.phase == SearchIndexPhase::Issues {
                                SubjectKind::Issue
                            } else {
                                SubjectKind::Pull
                            };
                            projection::refresh(context, Subject { kind, number })?;
                            build.after = number;
                            build.processed = build
                                .processed
                                .checked_add(1)
                                .filter(|count| *count <= MAX_NUMBER)
                                .ok_or(crate::cells::Error::Invalid(
                                    "discussion build count is exhausted",
                                ))?;
                        }
                    }
                }
                build.version = build
                    .version
                    .checked_add(1)
                    .filter(|version| *version <= MAX_NUMBER)
                    .ok_or(crate::cells::Error::Invalid(
                        "discussion build version is exhausted",
                    ))?;
                if rows[0].rows.len() < BATCH {
                    match build.phase {
                        SearchIndexPhase::Clear => {
                            build.phase = SearchIndexPhase::Issues;
                            build.after = 0;
                        }
                        SearchIndexPhase::Issues => {
                            build.phase = SearchIndexPhase::Pulls;
                            build.after = 0;
                        }
                        SearchIndexPhase::Pulls => {
                            context.sql(&SqlBatch { statements: vec![
                                statement("DELETE FROM discussion_index_dirty", vec![]),
                                statement("UPDATE discussion_index_revision SET revision = 0 WHERE singleton = 1", vec![]),
                                statement("INSERT INTO discussion_fts(discussion_fts) VALUES ('integrity-check')", vec![]),
                            ] })?;
                            current.progress = None;
                        }
                    }
                }
                current
            }
        };
        context.put_document(STATE_KEY, &next)?;
        progress(context)
    }
}
fn progress(
    context: &Transaction<'_>,
) -> crate::cells::Result<MutationResult<RebuildSearchOutcome>> {
    Ok(MutationResult::Success(RebuildSearchOutcome::Progress {
        status: status(context)?,
    }))
}
fn conflict(
    context: &Transaction<'_>,
) -> crate::cells::Result<MutationResult<RebuildSearchOutcome>> {
    Ok(MutationResult::Rejected(RebuildSearchOutcome::Conflict {
        status: status(context)?,
    }))
}
