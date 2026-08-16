//! Backend-agnostic live control for mounted views.

use std::path::{Path, PathBuf};

use crate::core::error::{CrabError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountControlBackend {
    Nfs,
    #[cfg_attr(
        all(feature = "nfs", not(feature = "fuse")),
        allow(
            dead_code,
            reason = "NFS-only builds keep backend-agnostic control payloads without constructing FUSE"
        )
    )]
    Fuse,
}

impl MountControlBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nfs => "nfs",
            Self::Fuse => "fuse",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountControlStatus {
    pub backend: MountControlBackend,
    pub mountpoint: PathBuf,
    pub source: Option<String>,
    pub head_ref: Option<String>,
    pub read_only: bool,
    pub head_oid: Option<String>,
    pub pid: Option<u32>,
    #[cfg(feature = "nfs")]
    pub nfs_runtime: Option<crate::nfs_control::NfsRuntimeStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountControlListEntry {
    pub name: String,
    pub backend: Option<String>,
    pub mountpoint: String,
    pub source: String,
    pub head_ref: String,
    pub read_only: bool,
    pub pid: Option<u32>,
    pub start_time: Option<String>,
    pub log_path: Option<String>,
    pub control_endpoint: Option<String>,
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountControlUpdate {
    pub backend: MountControlBackend,
    pub mountpoint: PathBuf,
    pub head_oid: Option<String>,
    pub head_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountControlShutdown {
    pub backend: MountControlBackend,
    pub mountpoint: PathBuf,
    pub mountpoint_str: String,
    pub registry_path: Option<PathBuf>,
    pub pid: Option<u32>,
}

#[cfg(feature = "nfs")]
#[derive(Debug, Clone)]
pub struct NfsMountControlContext {
    pub mountpoint: PathBuf,
    pub mountpoint_str: String,
    pub registry_path: PathBuf,
    pub entry: crate::mounts_registry::MountEntry,
    pub endpoint: String,
}

pub async fn list() -> Result<Vec<MountControlListEntry>> {
    let mut entries = persisted_list()?;

    #[cfg(feature = "nfs")]
    refresh_nfs_list_entries(&mut entries).await;

    #[cfg(feature = "fuse")]
    refresh_fuse_list_entries(&mut entries).await;

    Ok(entries)
}

pub fn persisted_list() -> Result<Vec<MountControlListEntry>> {
    let registry_path = crate::mounts_registry::registry_path()?;
    let entries = crate::mounts_registry::list_entries(&registry_path)?
        .into_iter()
        .map(MountControlListEntry::from_registry)
        .collect();
    Ok(entries)
}

pub async fn status(path: &Path) -> Result<Option<MountControlStatus>> {
    #[cfg(feature = "nfs")]
    if let Some(context) = nfs_context(path)? {
        let live = crate::nfs_control::status(&context.endpoint).await?;
        let head_ref = live
            .head_ref
            .clone()
            .or_else(|| Some(context.entry.git_ref.clone()));
        return Ok(Some(MountControlStatus {
            backend: MountControlBackend::Nfs,
            mountpoint: context.mountpoint,
            source: Some(context.entry.source),
            head_ref,
            read_only: live.read_only,
            head_oid: live.head_oid,
            pid: Some(live.pid),
            nfs_runtime: Some(live.runtime),
        }));
    }

    #[cfg(feature = "fuse")]
    {
        let mountpoint = normalize_mountpoint(path);
        let response = send_fuse_request(crate::ipc_server::IpcRequest::Status {
            mountpoint: mountpoint.display().to_string(),
        })
        .await?;
        let Some(status) = response.status else {
            return Err(CrabError::Internal(
                "mount control status response missing status".into(),
            ));
        };
        return Ok(Some(MountControlStatus {
            backend: MountControlBackend::Fuse,
            mountpoint,
            source: Some(status.remote),
            head_ref: Some(status.r#ref),
            read_only: status.read_only,
            head_oid: status.head_oid,
            pid: status.pid,
            #[cfg(feature = "nfs")]
            nfs_runtime: None,
        }));
    }

    #[cfg(not(feature = "fuse"))]
    {
        let _ = path;
        Ok(None)
    }
}

impl MountControlListEntry {
    fn from_registry(entry: crate::mounts_registry::MountEntry) -> Self {
        Self {
            name: entry.name,
            backend: entry.backend,
            mountpoint: entry.mountpoint,
            source: entry.source,
            head_ref: entry.git_ref,
            read_only: entry.read_only,
            pid: Some(entry.pid),
            start_time: Some(entry.start_time),
            log_path: entry.log_path,
            control_endpoint: entry.control_endpoint,
            live: false,
        }
    }
}

#[cfg(any(feature = "fuse", test))]
fn upsert_list_entry(entries: &mut Vec<MountControlListEntry>, live: MountControlListEntry) {
    if let Some(existing) = entries
        .iter_mut()
        .find(|entry| entry.mountpoint == live.mountpoint)
    {
        existing.backend = live.backend;
        existing.source = live.source;
        existing.head_ref = live.head_ref;
        existing.read_only = live.read_only;
        existing.pid = live.pid.or(existing.pid);
        existing.live = true;
        return;
    }
    entries.push(live);
}

#[cfg(feature = "nfs")]
async fn refresh_nfs_list_entries(entries: &mut [MountControlListEntry]) {
    for entry in entries.iter_mut() {
        if entry.backend.as_deref() != Some(MountControlBackend::Nfs.as_str()) {
            continue;
        }
        let Some(endpoint) = entry.control_endpoint.as_deref() else {
            continue;
        };
        let Ok(status) = crate::nfs_control::status(endpoint).await else {
            continue;
        };
        entry.live = true;
        entry.pid = Some(status.pid);
        entry.read_only = status.read_only;
        if let Some(head_ref) = status.head_ref {
            entry.head_ref = head_ref;
        }
    }
}

#[cfg(feature = "fuse")]
async fn refresh_fuse_list_entries(entries: &mut Vec<MountControlListEntry>) {
    let Ok(response) = send_fuse_request(crate::ipc_server::IpcRequest::List).await else {
        return;
    };
    let Some(mounts) = response.mounts else {
        return;
    };
    for mount in mounts {
        let status = fuse_status_for_mountpoint(&mount.mountpoint).await;
        let pid = status.as_ref().and_then(|status| status.pid);
        let head_ref = status
            .as_ref()
            .map(|status| status.r#ref.clone())
            .unwrap_or(mount.r#ref);
        let source = status
            .as_ref()
            .map(|status| status.remote.clone())
            .unwrap_or(mount.remote);
        let read_only = status
            .as_ref()
            .map_or(mount.read_only, |status| status.read_only);
        upsert_list_entry(
            entries,
            MountControlListEntry {
                name: crate::mounts_registry::derive_name_from_source(&source),
                backend: Some(MountControlBackend::Fuse.as_str().to_owned()),
                mountpoint: mount.mountpoint,
                source,
                head_ref,
                read_only,
                pid,
                start_time: None,
                log_path: None,
                control_endpoint: None,
                live: true,
            },
        );
    }
}

#[cfg(feature = "fuse")]
async fn fuse_status_for_mountpoint(mountpoint: &str) -> Option<crate::ipc_server::MountStatus> {
    let response = send_fuse_request(crate::ipc_server::IpcRequest::Status {
        mountpoint: mountpoint.to_owned(),
    })
    .await
    .ok()?;
    response.status
}

pub async fn refresh(path: &Path) -> Result<Option<MountControlUpdate>> {
    #[cfg(feature = "nfs")]
    if let Some(context) = nfs_context(path)? {
        let update = crate::nfs_control::refresh(&context.endpoint).await?;
        return Ok(Some(MountControlUpdate {
            backend: MountControlBackend::Nfs,
            mountpoint: context.mountpoint,
            head_oid: Some(update.head_oid),
            head_ref: Some(update.head_ref),
        }));
    }

    #[cfg(feature = "fuse")]
    {
        let mountpoint = normalize_mountpoint(path);
        let response = send_fuse_request(crate::ipc_server::IpcRequest::Refresh {
            mountpoint: mountpoint.display().to_string(),
        })
        .await?;
        return Ok(Some(MountControlUpdate {
            backend: MountControlBackend::Fuse,
            mountpoint,
            head_oid: response.head_oid,
            head_ref: None,
        }));
    }

    #[cfg(not(feature = "fuse"))]
    {
        let _ = path;
        Ok(None)
    }
}

pub async fn switch_ref(path: &Path, git_ref: &str) -> Result<Option<MountControlUpdate>> {
    #[cfg(feature = "nfs")]
    if let Some(mut context) = nfs_context(path)? {
        let update = crate::nfs_control::switch_ref(&context.endpoint, git_ref).await?;
        context.entry.git_ref.clone_from(&update.head_ref);
        crate::mounts_registry::add_entry(&context.registry_path, context.entry)?;
        return Ok(Some(MountControlUpdate {
            backend: MountControlBackend::Nfs,
            mountpoint: context.mountpoint,
            head_oid: Some(update.head_oid),
            head_ref: Some(update.head_ref),
        }));
    }

    #[cfg(feature = "fuse")]
    {
        let mountpoint = normalize_mountpoint(path);
        let response = send_fuse_request(crate::ipc_server::IpcRequest::SwitchRef {
            mountpoint: mountpoint.display().to_string(),
            r#ref: git_ref.to_owned(),
        })
        .await?;
        return Ok(Some(MountControlUpdate {
            backend: MountControlBackend::Fuse,
            mountpoint,
            head_oid: response.head_oid,
            head_ref: Some(git_ref.to_owned()),
        }));
    }

    #[cfg(not(feature = "fuse"))]
    {
        let _ = (path, git_ref);
        Ok(None)
    }
}

pub async fn commit(
    path: &Path,
    message: &str,
    push: bool,
) -> Result<Option<crate::publish::OverlayCommitResult>> {
    #[cfg(feature = "nfs")]
    if let Some(context) = nfs_context(path)? {
        let result = crate::nfs_control::commit(&context.endpoint, message, push).await?;
        return Ok(Some(result));
    }

    #[cfg(not(feature = "fuse"))]
    {
        let _ = (path, message, push);
        Ok(None)
    }

    #[cfg(feature = "fuse")]
    {
        let mountpoint = normalize_mountpoint(path);
        if fuse_status_for_mountpoint(&mountpoint.display().to_string())
            .await
            .is_none()
        {
            return Ok(None);
        }
        let response = send_fuse_request_with_timeout(
            crate::ipc_server::IpcRequest::CommitOverlay {
                mountpoint: mountpoint.display().to_string(),
                message: message.to_owned(),
                push,
            },
            std::time::Duration::from_secs(30 * 60),
        )
        .await?;
        if !response.ok {
            return Err(CrabError::Internal(
                response
                    .error
                    .unwrap_or_else(|| "FUSE mount commit failed".to_owned()),
            ));
        }
        let result = response
            .overlay_commit_result
            .ok_or_else(|| CrabError::Internal("FUSE commit response missing result".into()))?;
        Ok(Some(result))
    }
}

pub async fn reset_overlay(path: &Path) -> Result<Option<crate::publish::OverlayDiff>> {
    #[cfg(feature = "nfs")]
    if let Some(context) = nfs_context(path)? {
        let diff = crate::nfs_control::reset_overlay(&context.endpoint).await?;
        // Reset changes the namespace outside the kernel client. Wait out the
        // configured attribute TTL so a cached negative lookup cannot survive
        // a successful command response.
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        return Ok(Some(diff));
    }

    let _ = path;
    Ok(None)
}

pub async fn shutdown(path: &Path) -> Result<Option<MountControlShutdown>> {
    #[cfg(feature = "nfs")]
    if let Some(context) = nfs_context(path)? {
        crate::nfs_control::shutdown(&context.endpoint).await?;
        return Ok(Some(MountControlShutdown {
            backend: MountControlBackend::Nfs,
            mountpoint: context.mountpoint,
            mountpoint_str: context.mountpoint_str,
            registry_path: Some(context.registry_path),
            pid: Some(context.entry.pid),
        }));
    }

    #[cfg(feature = "fuse")]
    {
        let mountpoint = normalize_mountpoint(path);
        let mountpoint_str = mountpoint.display().to_string();
        let _response = send_fuse_request_with_timeout(
            crate::ipc_server::IpcRequest::Unmount {
                mountpoint: mountpoint_str.clone(),
            },
            std::time::Duration::from_secs(120),
        )
        .await?;
        return Ok(Some(MountControlShutdown {
            backend: MountControlBackend::Fuse,
            mountpoint,
            mountpoint_str,
            registry_path: None,
            pid: None,
        }));
    }

    #[cfg(not(feature = "fuse"))]
    {
        let _ = path;
        Ok(None)
    }
}

pub fn normalize_mountpoint(path: &Path) -> PathBuf {
    #[cfg(all(any(windows, test), feature = "nfs"))]
    if let Ok(target) = crate::nfs_mount::windows_mount_target(path) {
        return PathBuf::from(target);
    }

    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(feature = "nfs")]
pub fn nfs_context(path: &Path) -> Result<Option<NfsMountControlContext>> {
    let mountpoint = normalize_mountpoint(path);
    let mountpoint_str = mountpoint.display().to_string();
    let registry_path = crate::mounts_registry::registry_path()?;
    let Some(entry) = crate::mounts_registry::list_entries(&registry_path)?
        .into_iter()
        .find(|entry| entry.mountpoint == mountpoint_str)
    else {
        return Ok(None);
    };
    if entry.backend.as_deref() != Some(MountControlBackend::Nfs.as_str()) {
        return Ok(None);
    }
    let endpoint = entry
        .control_endpoint
        .clone()
        .ok_or_else(|| CrabError::Configuration {
            key: "NFS mount has no control endpoint".into(),
            origin: "crab mount control".into(),
        })?;
    Ok(Some(NfsMountControlContext {
        mountpoint,
        mountpoint_str,
        registry_path,
        entry,
        endpoint,
    }))
}

#[cfg(feature = "fuse")]
async fn send_fuse_request(
    request: crate::ipc_server::IpcRequest,
) -> Result<crate::ipc_server::IpcResponse> {
    send_fuse_request_with_timeout(request, std::time::Duration::from_secs(30)).await
}

#[cfg(feature = "fuse")]
async fn send_fuse_request_with_timeout(
    request: crate::ipc_server::IpcRequest,
    timeout: std::time::Duration,
) -> Result<crate::ipc_server::IpcResponse> {
    let socket_path = crate::ipc_client::default_socket_path()?;
    let mut client = crate::ipc_client::IpcClient::connect(&socket_path)
        .await
        .map_err(CrabError::from)?;
    let response = client
        .send_with_timeout(&request, timeout)
        .await
        .map_err(CrabError::from)?;
    if response.ok {
        return Ok(response);
    }
    Err(CrabError::Internal(response.error.unwrap_or_else(|| {
        "mount control request failed".to_owned()
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "nfs")]
    use std::sync::Arc;

    #[cfg(feature = "nfs")]
    use tokio_util::sync::CancellationToken;

    struct HomeGuard {
        original: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn set(home: &Path) -> Self {
            let original = std::env::var_os("HOME");
            // SAFETY: these tests update HOME before starting any worker
            // threads and restore it before returning to the harness.
            unsafe {
                std::env::set_var("HOME", home);
            }
            Self { original }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            // SAFETY: restores the process environment for this single-threaded
            // test scope.
            unsafe {
                if let Some(original) = &self.original {
                    std::env::set_var("HOME", original);
                } else {
                    std::env::remove_var("HOME");
                }
            }
        }
    }

    #[test]
    fn backend_names_match_registry_values() {
        assert_eq!(MountControlBackend::Nfs.as_str(), "nfs");
        assert_eq!(MountControlBackend::Fuse.as_str(), "fuse");
    }

    #[test]
    fn normalize_preserves_missing_paths() {
        let path = Path::new("/tmp/crab-missing-mount-control-test");
        assert_eq!(normalize_mountpoint(path), path);
    }

    #[test]
    fn persisted_list_reads_registry_entries() {
        let _env_lock = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let mountpoint = tmp.path().join("view");
        let registry_path = crate::mounts_registry::registry_path().unwrap();
        crate::mounts_registry::add_entry(
            &registry_path,
            crate::mounts_registry::MountEntry {
                mountpoint: mountpoint.display().to_string(),
                source: "crab://bucket/repo".to_owned(),
                git_ref: "refs/heads/main".to_owned(),
                pid: 123,
                start_time: "2026-01-01T00:00:00Z".to_owned(),
                read_only: true,
                name: "repo".to_owned(),
                backend: Some("nfs".to_owned()),
                log_path: Some("/tmp/crab-nfs.log".to_owned()),
                control_endpoint: Some("unix:/tmp/crab-nfs.sock".to_owned()),
            },
        )
        .unwrap();

        let entries = persisted_list().unwrap();

        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.name, "repo");
        assert_eq!(entry.backend.as_deref(), Some("nfs"));
        assert_eq!(entry.mountpoint, mountpoint.display().to_string());
        assert_eq!(entry.source, "crab://bucket/repo");
        assert_eq!(entry.head_ref, "refs/heads/main");
        assert!(entry.read_only);
        assert_eq!(entry.pid, Some(123));
        assert_eq!(entry.start_time.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(entry.log_path.as_deref(), Some("/tmp/crab-nfs.log"));
        assert_eq!(
            entry.control_endpoint.as_deref(),
            Some("unix:/tmp/crab-nfs.sock")
        );
        assert!(!entry.live);
    }

    #[test]
    fn list_keeps_registry_entries_when_live_probe_fails() {
        let _env_lock = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let mountpoint = tmp.path().join("view");
        let registry_path = crate::mounts_registry::registry_path().unwrap();
        crate::mounts_registry::add_entry(
            &registry_path,
            crate::mounts_registry::MountEntry {
                mountpoint: mountpoint.display().to_string(),
                source: "crab://bucket/repo".to_owned(),
                git_ref: "refs/heads/main".to_owned(),
                pid: 123,
                start_time: "2026-01-01T00:00:00Z".to_owned(),
                read_only: true,
                name: "repo".to_owned(),
                backend: Some("nfs".to_owned()),
                log_path: None,
                control_endpoint: Some("unix:/tmp/crab-nfs-missing.sock".to_owned()),
            },
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let entries = runtime.block_on(list()).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mountpoint, mountpoint.display().to_string());
        assert!(!entries[0].live);
    }

    #[test]
    fn upsert_list_entry_preserves_registry_metadata() {
        let mut entries = vec![MountControlListEntry {
            name: "repo".to_owned(),
            backend: Some("nfs".to_owned()),
            mountpoint: "/mnt/repo".to_owned(),
            source: "crab://bucket/repo".to_owned(),
            head_ref: "refs/heads/main".to_owned(),
            read_only: true,
            pid: Some(123),
            start_time: Some("2026-01-01T00:00:00Z".to_owned()),
            log_path: Some("/tmp/crab-nfs.log".to_owned()),
            control_endpoint: Some("unix:/tmp/crab-nfs.sock".to_owned()),
            live: false,
        }];

        upsert_list_entry(
            &mut entries,
            MountControlListEntry {
                name: "ignored".to_owned(),
                backend: Some("fuse".to_owned()),
                mountpoint: "/mnt/repo".to_owned(),
                source: "crab://bucket/repo-live".to_owned(),
                head_ref: "refs/heads/dev".to_owned(),
                read_only: false,
                pid: Some(456),
                start_time: None,
                log_path: None,
                control_endpoint: None,
                live: true,
            },
        );

        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.name, "repo");
        assert_eq!(entry.backend.as_deref(), Some("fuse"));
        assert_eq!(entry.source, "crab://bucket/repo-live");
        assert_eq!(entry.head_ref, "refs/heads/dev");
        assert!(!entry.read_only);
        assert_eq!(entry.pid, Some(456));
        assert_eq!(entry.start_time.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(entry.log_path.as_deref(), Some("/tmp/crab-nfs.log"));
        assert_eq!(
            entry.control_endpoint.as_deref(),
            Some("unix:/tmp/crab-nfs.sock")
        );
        assert!(entry.live);
    }

    #[cfg(feature = "nfs")]
    fn nfs_control_state(
        mountpoint: &Path,
        read_only: bool,
    ) -> crate::nfs_control::NfsControlState {
        crate::nfs_control::NfsControlState {
            mountpoint: mountpoint.to_path_buf(),
            read_only,
            read_leases: crate::read_lease_pool::ReadLeasePool::new(4, 4096),
            directory_pages: crate::nfs::NfsDirectoryPageCache::new(4, 4096),
            write_journal: Arc::new(crate::nfs::NfsWriteJournal::new()),
            protocol_stats: Arc::new(crate::nfs::NfsProtocolStats::new()),
            engine: None,
            runtime: None,
            lifecycle: crate::nfs_control::NfsMountLifecycleStatus {
                server_bind_ms: 3,
                native_mount_ms: 5,
                startup_ms: 8,
            },
        }
    }

    #[cfg(feature = "nfs")]
    fn free_tcp_control_endpoint() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("tcp:{addr}?token=mount-control-test")
    }

    #[cfg(feature = "nfs")]
    fn add_nfs_mount_entry(
        registry_path: &Path,
        mountpoint: &Path,
        endpoint: &str,
    ) -> crate::mounts_registry::MountEntry {
        let entry = crate::mounts_registry::MountEntry {
            mountpoint: mountpoint.display().to_string(),
            source: "crab://bucket/repo".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            pid: 123,
            start_time: "2026-01-01T00:00:00Z".to_owned(),
            read_only: true,
            name: "repo".to_owned(),
            backend: Some("nfs".to_owned()),
            log_path: Some("/tmp/crab-nfs.log".to_owned()),
            control_endpoint: Some(endpoint.to_owned()),
        };
        crate::mounts_registry::add_entry(registry_path, entry.clone()).unwrap();
        entry
    }

    #[cfg(feature = "nfs")]
    async fn wait_for_nfs_status(path: &Path) -> MountControlStatus {
        for _ in 0..20 {
            if let Ok(Some(status)) = status(path).await {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        status(path).await.unwrap().unwrap()
    }

    #[test]
    #[cfg(feature = "nfs")]
    fn nfs_control_status_and_shutdown_route_through_registry_endpoint() {
        let _env_lock = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let raw_mountpoint = tmp.path().join("view");
        std::fs::create_dir_all(&raw_mountpoint).unwrap();
        let mountpoint = normalize_mountpoint(&raw_mountpoint);
        let registry_path = crate::mounts_registry::registry_path().unwrap();
        let endpoint = free_tcp_control_endpoint();
        add_nfs_mount_entry(&registry_path, &mountpoint, &endpoint);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let cancel = CancellationToken::new();
            let handle = crate::nfs_control::spawn_server(
                Some(endpoint.clone()),
                nfs_control_state(&mountpoint, false),
                cancel.clone(),
            )
            .unwrap();

            let status = wait_for_nfs_status(&mountpoint).await;

            assert_eq!(status.backend, MountControlBackend::Nfs);
            assert_eq!(status.mountpoint, mountpoint);
            assert_eq!(status.source.as_deref(), Some("crab://bucket/repo"));
            assert_eq!(status.head_ref.as_deref(), Some("refs/heads/main"));
            assert!(!status.read_only);
            assert_eq!(status.pid, Some(std::process::id()));
            assert_eq!(
                status.nfs_runtime.unwrap().lifecycle,
                crate::nfs_control::NfsMountLifecycleStatus {
                    server_bind_ms: 3,
                    native_mount_ms: 5,
                    startup_ms: 8,
                }
            );

            let shutdown = shutdown(&mountpoint).await.unwrap().unwrap();

            assert_eq!(shutdown.backend, MountControlBackend::Nfs);
            assert_eq!(shutdown.mountpoint, mountpoint);
            assert_eq!(
                shutdown.mountpoint_str,
                shutdown.mountpoint.display().to_string()
            );
            assert_eq!(
                shutdown.registry_path.as_deref(),
                Some(registry_path.as_path())
            );
            assert_eq!(shutdown.pid, Some(123));
            assert!(cancel.is_cancelled());
            handle.await.unwrap().unwrap();
        });
    }

    #[test]
    #[cfg(feature = "nfs")]
    fn nfs_control_list_refreshes_live_registry_entries() {
        let _env_lock = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let raw_mountpoint = tmp.path().join("view");
        std::fs::create_dir_all(&raw_mountpoint).unwrap();
        let mountpoint = normalize_mountpoint(&raw_mountpoint);
        let registry_path = crate::mounts_registry::registry_path().unwrap();
        let endpoint = free_tcp_control_endpoint();
        add_nfs_mount_entry(&registry_path, &mountpoint, &endpoint);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let cancel = CancellationToken::new();
            let handle = crate::nfs_control::spawn_server(
                Some(endpoint.clone()),
                nfs_control_state(&mountpoint, false),
                cancel.clone(),
            )
            .unwrap();
            wait_for_nfs_status(&mountpoint).await;

            let entries = list().await.unwrap();

            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].mountpoint, mountpoint.display().to_string());
            assert_eq!(entries[0].backend.as_deref(), Some("nfs"));
            assert_eq!(entries[0].pid, Some(std::process::id()));
            assert!(!entries[0].read_only);
            assert!(entries[0].live);

            crate::nfs_control::shutdown(&endpoint).await.unwrap();
            handle.await.unwrap().unwrap();
            assert!(cancel.is_cancelled());
        });
    }

    #[test]
    #[cfg(feature = "nfs")]
    fn nfs_control_refresh_and_switch_route_to_helper_endpoint() {
        let _env_lock = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let raw_mountpoint = tmp.path().join("view");
        std::fs::create_dir_all(&raw_mountpoint).unwrap();
        let mountpoint = normalize_mountpoint(&raw_mountpoint);
        let registry_path = crate::mounts_registry::registry_path().unwrap();
        let endpoint = free_tcp_control_endpoint();
        add_nfs_mount_entry(&registry_path, &mountpoint, &endpoint);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let cancel = CancellationToken::new();
            let handle = crate::nfs_control::spawn_server(
                Some(endpoint.clone()),
                nfs_control_state(&mountpoint, true),
                cancel.clone(),
            )
            .unwrap();
            wait_for_nfs_status(&mountpoint).await;

            let refresh_error = refresh(&mountpoint).await.unwrap_err().to_string();
            let switch_error = switch_ref(&mountpoint, "refs/heads/dev")
                .await
                .unwrap_err()
                .to_string();

            assert!(refresh_error.contains("NFS control refresh unavailable"));
            assert!(switch_error.contains("NFS control switch unavailable"));
            assert!(!cancel.is_cancelled());

            crate::nfs_control::shutdown(&endpoint).await.unwrap();
            handle.await.unwrap().unwrap();
        });
    }

    #[test]
    #[cfg(feature = "nfs")]
    fn nfs_context_uses_registry_backend_and_endpoint() {
        let _env_lock = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let mountpoint = tmp.path().join("view");
        let registry_path = crate::mounts_registry::registry_path().unwrap();
        crate::mounts_registry::add_entry(
            &registry_path,
            crate::mounts_registry::MountEntry {
                mountpoint: mountpoint.display().to_string(),
                source: "crab://bucket/repo".to_owned(),
                git_ref: "refs/heads/main".to_owned(),
                pid: 123,
                start_time: "2026-01-01T00:00:00Z".to_owned(),
                read_only: true,
                name: "repo".to_owned(),
                backend: Some("nfs".to_owned()),
                log_path: Some("/tmp/crab-nfs.log".to_owned()),
                control_endpoint: Some("unix:/tmp/crab-nfs.sock".to_owned()),
            },
        )
        .unwrap();

        let context = nfs_context(&mountpoint).unwrap().unwrap();

        assert_eq!(context.mountpoint, mountpoint);
        assert_eq!(context.endpoint, "unix:/tmp/crab-nfs.sock");
        assert_eq!(context.entry.pid, 123);
    }

    #[test]
    #[cfg(feature = "nfs")]
    fn nfs_context_ignores_non_nfs_registry_entries() {
        let _env_lock = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let mountpoint = tmp.path().join("view");
        let registry_path = crate::mounts_registry::registry_path().unwrap();
        crate::mounts_registry::add_entry(
            &registry_path,
            crate::mounts_registry::MountEntry {
                mountpoint: mountpoint.display().to_string(),
                source: "crab://bucket/repo".to_owned(),
                git_ref: "refs/heads/main".to_owned(),
                pid: 123,
                start_time: "2026-01-01T00:00:00Z".to_owned(),
                read_only: true,
                name: "repo".to_owned(),
                backend: Some("fuse".to_owned()),
                log_path: None,
                control_endpoint: Some("unix:/tmp/crab-nfs.sock".to_owned()),
            },
        )
        .unwrap();

        assert!(nfs_context(&mountpoint).unwrap().is_none());
    }
}
