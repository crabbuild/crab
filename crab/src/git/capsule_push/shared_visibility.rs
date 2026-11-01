//! Adapt exact physical Git closures into authenticated shared transaction evidence.
use super::{BTreeMap, BTreeSet, CancellationToken, CrabError, Path, Result, check_cancelled};
use crab_metadata::git_visibility::{GitVisibilityDictionary, GitVisibilityObjects};

pub(super) fn prepare_replacements(
    git_dir: &Path,
    edits: &[crab_metadata::capsule_protocol::CapsuleRefEdit],
    fast_forwards: &BTreeSet<String>,
    available_old_tips: &BTreeSet<String>,
    cancel: &CancellationToken,
) -> Result<BTreeMap<String, GitVisibilityObjects>> {
    let replacements = edits
        .iter()
        .filter_map(|edit| {
            let new = edit.new_oid()?;
            let incremental = edit.expected_old().is_some_and(|old| {
                fast_forwards.contains(edit.ref_name()) || available_old_tips.contains(old)
            });
            (!incremental).then_some((edit, new))
        })
        .collect::<Vec<_>>();
    if replacements.len() < 2 {
        return Ok(BTreeMap::new());
    }
    let roots = replacements
        .iter()
        .map(|(edit, new)| {
            gix_hash::ObjectId::from_hex(new.as_bytes()).map_err(|error| {
                CrabError::Protocol(format!(
                    "invalid replacement tip for {}: {error}",
                    edit.ref_name()
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let started = std::time::Instant::now();
    let graph = crab_git::shared_visibility::build(
        git_dir,
        &roots,
        crab_git::shared_visibility::Limits {
            max_objects: usize::try_from(crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS)
                .map_err(|_| {
                    CrabError::Internal("Git visibility limit does not fit usize".into())
                })?,
            ..Default::default()
        },
        || cancel.is_cancelled(),
    )
    .map_err(|error| match error {
        crab_git::shared_visibility::Error::Cancelled => CrabError::Cancelled,
        error => CrabError::Io(std::io::Error::other(error)),
    })?;
    let dictionary =
        GitVisibilityDictionary::new(graph.objects().iter().map(ToString::to_string).collect())?;
    let mut result = BTreeMap::new();
    for ((edit, _), closure) in replacements.into_iter().zip(graph.closures()) {
        check_cancelled(cancel)?;
        let objects = GitVisibilityObjects::from_shared_chunks(
            dictionary.clone(),
            closure.chunks().to_vec(),
        )?;
        result.insert(edit.ref_name().to_owned(), objects);
    }
    tracing::debug!(
        refs = result.len(),
        distinct_objects = graph.objects().len(),
        structural_bytes = graph.structural_bytes,
        links = graph.distinct_links,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "prepared exact shared Git visibility"
    );
    Ok(result)
}
