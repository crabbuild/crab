use super::*;

impl CacheCatalog {
    /// Persist a root-wide retention limit without interrupting active reads or fills.
    ///
    /// The limit constrains future admissions and maintenance in addition to each
    /// caller's ceiling. This does not run maintenance; a busy maintenance owner,
    /// unsafe root or unavailable catalog returns an error without changing policy.
    pub async fn set_retention_budget(&self, max_bytes: Option<u64>) -> Result<()> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            let pinned = PinnedRoot::create(&root)?;
            let owner = pinned.open_lock(Path::new(MAINTENANCE_LOCK))?;
            // A pass already admitted under the old policy must finish before
            // the update commits. Keep the same lock order as maintenance.
            if !owner.try_lock_exclusive()? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "cache maintenance is already running",
                )
                .into());
            }
            let path = root.join(CATALOG_FILE);
            let mut connection = open_catalog(&pinned, &path)?;
            // A confirmed user policy must survive more than process exit.
            // Keep disposable accounting on NORMAL; sync this authority commit.
            connection
                .pragma_update(None, "synchronous", "FULL")
                .map_err(|source| index_error(&path, source))?;
            let transaction = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|source| index_error(&path, source))?;
            let value = max_bytes.map_or_else(|| "unlimited".to_owned(), |bytes| bytes.to_string());
            transaction
                .execute(
                    "INSERT INTO catalog_meta(key, value) VALUES ('retention_budget_bytes', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    [&value],
                )
                .map_err(|source| index_error(&path, source))?;
            transaction
                .commit()
                .map_err(|source| index_error(&path, source))
        })
        .await
        .map_err(|error| CacheError::Io(std::io::Error::other(error)))?
    }
}

pub(super) fn retention_budget(
    connection: &rusqlite::Connection,
    path: &Path,
) -> Result<Option<u64>> {
    connection
        .query_row(
            "SELECT value FROM catalog_meta WHERE key = 'retention_budget_bytes'",
            [],
            |row| {
                let value = row.get_ref(0)?;
                let value = value.as_str()?;
                if value == "unlimited" {
                    return Ok(None);
                }
                value.parse::<u64>().map(Some).map_err(|source| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(source),
                    )
                })
            },
        )
        .optional()
        .map(Option::flatten)
        .map_err(|source| index_error(path, source))
}

pub(super) fn effective_budget(
    connection: &rusqlite::Connection,
    path: &Path,
    caller_limit: Option<u64>,
) -> Result<Option<u64>> {
    Ok(match (caller_limit, retention_budget(connection, path)?) {
        (Some(caller), Some(root)) => Some(caller.min(root)),
        (caller, root) => caller.or(root),
    })
}
