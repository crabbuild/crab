use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use super::CellPagedDatabase;
use crate::{CrabError, Host, Position, Result};

pub(super) async fn run(database: &CellPagedDatabase, destination: &Path) -> Result<Position> {
    let host = database.replica.host.for_recovery().await?;
    crate::recovery::reject_sidecars(destination, &host)?;
    if host.filesystem.exists(destination)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "restore destination already exists",
        )
        .into());
    }
    let (mut scratch, mut file) = RestoreScratch::create(&host, destination)?;
    let mut database = database.clone();
    database.replica = database.replica.with_host(host.clone());
    let lock = crate::ltx::lock_pgno(database.page_size);
    let mut page = 1;
    let mut checksum = crate::CHECKSUM_FLAG;
    while page <= database.database_pages {
        let pages = if page == lock {
            vec![(page, vec![0; database.page_size as usize])]
        } else {
            database.read_run(page, u32::MAX).await?
        };
        if pages.is_empty()
            || pages[0].0 != page
            || pages
                .windows(2)
                .any(|pair| pair[0].0.checked_add(1) != Some(pair[1].0))
        {
            return Err(CrabError::LTXCorrupted);
        }
        for (number, bytes) in &pages {
            if *number != lock {
                checksum =
                    (checksum ^ crate::ltx::checksum_page(*number, bytes)) | crate::CHECKSUM_FLAG;
            }
        }
        let last = pages
            .last()
            .map(|(number, _)| *number)
            .ok_or(CrabError::LTXCorrupted)?;
        file = host
            .run(move || {
                for (_, bytes) in pages {
                    file.write_all(&bytes)?;
                }
                Ok::<_, CrabError>(file)
            })
            .await??;
        if last == database.database_pages {
            break;
        }
        page = last.checked_add(1).ok_or(CrabError::LTXCorrupted)?;
    }
    if checksum != database.position.checksum {
        return Err(CrabError::ChecksumMismatch);
    }
    let expected_bytes = u64::from(database.page_size) * u64::from(database.database_pages);
    host.run(move || {
        if file.file_len()? != expected_bytes {
            return Err(CrabError::LTXCorrupted);
        }
        file.sync_all()?;
        Ok::<_, CrabError>(())
    })
    .await??;
    let filesystem = Arc::clone(&host.filesystem);
    let source = scratch.path.clone();
    let destination = destination.to_owned();
    host.run(move || {
        filesystem.persist_file_new(&source, &destination)?;
        Ok::<_, CrabError>(())
    })
    .await??;
    scratch.installed = true;
    Ok(database.position)
}

struct RestoreScratch {
    filesystem: Arc<dyn crate::environment::FileSystem>,
    path: PathBuf,
    installed: bool,
}

impl RestoreScratch {
    fn create(
        host: &Host,
        destination: &Path,
    ) -> Result<(Self, Box<dyn crate::environment::FileIo>)> {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let filename = destination
            .file_name()
            .ok_or(CrabError::InvalidState("missing restore filename"))?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        for _ in 0..16 {
            let mut scratch_name = filename.to_owned();
            scratch_name.push(format!(
                ".crab-restore-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let path = parent.join(scratch_name);
            match host.filesystem.create(&path) {
                Ok(file) => {
                    return Ok((
                        Self {
                            filesystem: Arc::clone(&host.filesystem),
                            path,
                            installed: false,
                        },
                        file,
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "restore scratch namespace exhausted",
        )
        .into())
    }
}

impl Drop for RestoreScratch {
    fn drop(&mut self) {
        if !self.installed {
            let _ = self.filesystem.remove_file(&self.path);
        }
    }
}
