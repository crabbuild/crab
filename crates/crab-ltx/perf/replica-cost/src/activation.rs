use crab_ltx::{CellReplica, Db, RootRef};
use serde::Serialize;
use std::path::Path;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Activation {
    #[default]
    Fresh,
    Sparse,
    Hydrated,
    Resumed,
}

impl FromStr for Activation {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "fresh" => Ok(Self::Fresh),
            "sparse" => Ok(Self::Sparse),
            "hydrated" => Ok(Self::Hydrated),
            "resumed" => Ok(Self::Resumed),
            _ => Err("activation must be fresh, sparse, hydrated, or resumed"),
        }
    }
}

impl Activation {
    pub(super) async fn open(
        self,
        replica: &CellReplica,
        root: &RootRef,
        database: Db,
        directory: &Path,
    ) -> Result<Db, Box<dyn std::error::Error>> {
        if self == Self::Fresh {
            return Ok(database);
        }
        database.close()?;
        let active = directory.join("sparse.sqlite");
        let writable = replica
            .open_root(root)
            .await?
            .paged()
            .prepare_writable(&active)
            .await?;
        let mut database =
            tokio::task::spawn_blocking(move || writable.open_writable(&active)).await??;
        if self == Self::Sparse {
            return Ok(database);
        }
        while !database.hydration()?.is_some_and(|state| state.complete()) {
            let batch = database
                .prepare_hydration(64)?
                .ok_or("sparse activation lost hydration state")?
                .fetch()
                .await?;
            database.install_hydration(batch)?;
        }
        if self == Self::Hydrated {
            return Ok(database);
        }
        // This fixture owns the exact root and performs no intervening writes.
        // Resume still verifies every local page against its saved checksum.
        database.persist_continuation()?;
        let source = database.path().to_owned();
        database.close()?;
        Ok(replica.open_resumed(&source, &directory.join("resumed.sqlite"))?)
    }
}
