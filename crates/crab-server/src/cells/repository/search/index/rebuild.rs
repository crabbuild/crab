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
                    .is_some_and(|state| state.generation == generation && state.format == FORMAT)
                {
                    return progress(context);
                }
                if current.as_ref().map(|state| state.generation) != expected
                    || current
                        .as_ref()
                        .is_some_and(|state| state.progress.is_some() && state.format == FORMAT)
                {
                    return conflict(context);
                }
                if generation.is_nil() {
                    return Err(crate::cells::Error::Invalid(
                        "discussion index generation is invalid",
                    ));
                }
                // Preserve the existing TEXT lookup and its index. Clear/rebuild
                // removes old subjects in bounded frames before serving UUID keys.
                if current.as_ref().is_some_and(|state| state.format < 4) {
                    context.install_schema(
                        "ALTER TABLE discussion_index_assignees RENAME COLUMN subject TO actor;",
                    )?;
                }
                if current.as_ref().is_some_and(|state| state.format < 5) {
                    // Constant defaults avoid rewriting existing rows; bounded
                    // clear/reprojection fills names before format 5 can serve.
                    context.install_schema("ALTER TABLE discussion_index_assignees ADD COLUMN name TEXT NOT NULL DEFAULT ''; ALTER TABLE discussion_index_assignees ADD COLUMN name_key TEXT NOT NULL DEFAULT '';")?;
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
                if current.generation != generation || current.format != FORMAT {
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
                let done = match build.phase {
                    SearchIndexPhase::Milestones => {
                        use crate::cells::repository::collaboration::milestones::{
                            ListMilestones, ListMilestonesInput,
                        };
                        let page = ListMilestones::execute(
                            context,
                            ListMilestonesInput {
                                before: (build.after != 0).then_some(build.after),
                                limit: BATCH as u8,
                                state: None,
                            },
                        )?;
                        for milestone in page.items {
                            super::super::milestones::project(context, &milestone)?;
                            build.after = milestone.number;
                        }
                        page.next.is_none()
                    }
                    SearchIndexPhase::Clear | SearchIndexPhase::ClearMilestones => {
                        let sql = if build.phase == SearchIndexPhase::Clear {
                            "SELECT rowid FROM discussion_index ORDER BY rowid LIMIT 8"
                        } else {
                            "SELECT number FROM discussion_index_milestone_catalog ORDER BY number LIMIT 8"
                        };
                        let rows = context.query(sql, &[])?;
                        for row in &rows.rows {
                            let number = crate::cells::repository::result_u64_from_row(row, 0)?;
                            if build.phase == SearchIndexPhase::Clear {
                                projection::remove(context, number)?;
                            } else {
                                context.query("DELETE FROM discussion_index_milestone_catalog WHERE number = ?", &[integer(number)?])?;
                            }
                        }
                        rows.rows.len() < BATCH
                    }
                    SearchIndexPhase::Issues | SearchIndexPhase::Pulls => {
                        let (sql, kind) = if build.phase == SearchIndexPhase::Issues {
                            (
                                "SELECT number FROM repository_issues WHERE number > ? ORDER BY number LIMIT 8",
                                SubjectKind::Issue,
                            )
                        } else {
                            (
                                "SELECT number FROM repository_pulls WHERE number > ? ORDER BY number LIMIT 8",
                                SubjectKind::Pull,
                            )
                        };
                        let rows = context.query(sql, &[integer(build.after)?])?;
                        for row in &rows.rows {
                            let number = crate::cells::repository::result_u64_from_row(row, 0)?;
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
                        rows.rows.len() < BATCH
                    }
                };
                build.version = build
                    .version
                    .checked_add(1)
                    .filter(|version| *version <= MAX_NUMBER)
                    .ok_or(crate::cells::Error::Invalid(
                        "discussion build version is exhausted",
                    ))?;
                if done {
                    match build.phase {
                        SearchIndexPhase::Clear => {
                            build.phase = SearchIndexPhase::ClearMilestones;
                            build.after = 0;
                        }
                        SearchIndexPhase::ClearMilestones => {
                            build.phase = SearchIndexPhase::Milestones;
                            build.after = 0;
                        }
                        SearchIndexPhase::Milestones => {
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
