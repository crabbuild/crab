use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures_util::{StreamExt as _, stream};

use super::{CellPagedDatabase, FetchedSpan, RESTORE_IN_FLIGHT_WINDOWS, RESTORE_WINDOW_BYTES};
use crate::{CrabError, Host, Position, Result};

enum DownloadedWindow {
    Lock,
    Remote(Vec<FetchedSpan>),
}

pub(super) async fn run(database: &CellPagedDatabase, destination: &Path) -> Result<Position> {
    let scratch_bytes =
        crate::recovery::full_job_scratch_bytes(database.page_size, database.database_pages)?;
    let host = database
        .replica
        .host
        .for_recovery()
        .await?
        .for_scratch(scratch_bytes)
        .await?;
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
    // Worst-case frame sizing keeps every prefetched window within 1 MiB;
    // ordered installation decodes only one additional page at a time.
    let pages_per_window = RESTORE_WINDOW_BYTES
        .checked_div(crate::paged::maximum_frame_bytes(database.page_size)?)
        .filter(|pages| *pages > 0)
        .ok_or(CrabError::LTXCorrupted)?;
    let database_pages = database.database_pages;
    let page_size = database.page_size;
    let mut next_page = Some(1);
    let windows = std::iter::from_fn(move || {
        let first = next_page?;
        let last = if first == lock {
            first
        } else {
            let mut last = first
                .saturating_add(pages_per_window - 1)
                .min(database_pages);
            if first < lock {
                last = last.min(lock - 1);
            }
            last
        };
        next_page = if last == database_pages {
            None
        } else {
            last.checked_add(1)
        };
        Some((first, last - first + 1))
    });
    let download_database = database.clone();
    let mut downloads = stream::iter(windows.map(move |(first, count)| {
        let database = download_database.clone();
        async move {
            if first == lock {
                Ok::<_, CrabError>((first, count, DownloadedWindow::Lock))
            } else {
                Ok((
                    first,
                    count,
                    DownloadedWindow::Remote(database.read_restore_window(first, count).await?),
                ))
            }
        }
    }))
    .buffered(RESTORE_IN_FLIGHT_WINDOWS);
    let mut checksum = crate::CHECKSUM_FLAG;
    let mut expected_page = 1u64;
    while let Some(window) = downloads.next().await {
        let (first, count, downloaded) = window?;
        if u64::from(first) != expected_page {
            return Err(CrabError::LTXCorrupted);
        }
        let write_started = host.now_monotonic();
        let write = host
            .run(move || {
                let mut next = u64::from(first);
                let end = next + u64::from(count);
                let window_bytes = (count as usize)
                    .checked_mul(page_size as usize)
                    .filter(|bytes| *bytes <= RESTORE_WINDOW_BYTES as usize)
                    .ok_or(CrabError::LTXCorrupted)?;
                let mut window = Vec::with_capacity(window_bytes);
                match downloaded {
                    DownloadedWindow::Lock => {
                        window.resize(page_size as usize, 0);
                        next += 1;
                    }
                    DownloadedWindow::Remote(spans) => {
                        for span in spans {
                            span.try_for_each_page(page_size, |number, bytes| {
                                if u64::from(number) != next || next >= end {
                                    return Err(CrabError::LTXCorrupted);
                                }
                                checksum = (checksum ^ crate::ltx::checksum_page(number, &bytes))
                                    | crate::CHECKSUM_FLAG;
                                window.extend_from_slice(&bytes);
                                next += 1;
                                Ok(())
                            })?;
                        }
                    }
                }
                if next != end || window.len() != window_bytes {
                    return Err(CrabError::LTXCorrupted);
                }
                // Commit only fully verified windows to the private scratch file;
                // the 1 MiB window bound also caps this coalescing buffer.
                file.write_all(&window)?;
                Ok::<_, CrabError>((file, checksum))
            })
            .await;
        host.observe_ltx_phase(
            crate::LtxPhase::RestoreWrite,
            write_started,
            matches!(&write, Ok(Ok(_))),
        );
        let (next_file, next_checksum) = write??;
        file = next_file;
        checksum = next_checksum;
        expected_page += u64::from(count);
    }
    if expected_page != u64::from(database.database_pages) + 1 {
        return Err(CrabError::LTXCorrupted);
    }
    if checksum != database.position.checksum {
        return Err(CrabError::ChecksumMismatch);
    }
    let expected_bytes = u64::from(database.page_size) * u64::from(database.database_pages);
    let sync_started = host.now_monotonic();
    let sync = host
        .run(move || {
            if file.file_len()? != expected_bytes {
                return Err(CrabError::LTXCorrupted);
            }
            file.sync_all()?;
            Ok::<_, CrabError>(())
        })
        .await;
    host.observe_ltx_phase(
        crate::LtxPhase::RestoreWrite,
        sync_started,
        matches!(&sync, Ok(Ok(()))),
    );
    sync??;
    let filesystem = Arc::clone(&host.filesystem);
    let source = scratch.path.clone();
    let destination = destination.to_owned();
    let install_started = host.now_monotonic();
    let install = host
        .run(move || {
            filesystem.persist_file_new(&source, &destination)?;
            Ok::<_, CrabError>(())
        })
        .await;
    host.observe_ltx_phase(
        crate::LtxPhase::RestoreWrite,
        install_started,
        matches!(&install, Ok(Ok(()))),
    );
    install??;
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
