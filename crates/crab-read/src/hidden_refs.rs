pub(crate) fn compile(patterns: &[String]) -> globset::GlobSet {
    if patterns.is_empty() {
        return globset::GlobSet::empty();
    }

    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        match globset::Glob::new(pattern) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(error) => {
                tracing::warn!(
                    pattern = %pattern,
                    error = %error,
                    "transfer.hideRefs: invalid glob pattern; skipping"
                );
            }
        }
    }

    builder.build().unwrap_or_else(|error| {
        tracing::warn!(
            error = %error,
            "transfer.hideRefs: GlobSet build failed; no refs will be hidden"
        );
        globset::GlobSet::empty()
    })
}
