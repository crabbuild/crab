//! Backend-agnostic live control for mounted views.

use std::path::{Path, PathBuf};

use crate::core::error::{CrabError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountControlBackend {
    Nfs,
    #[cfg_attr(
        all(feature = "nfs", not(unix)),
        allow(
            dead_code,
            reason = "non-Unix NFS builds retain the backend-neutral payload"
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    {
        let mountpoint = normalize_mountpoint(path);
        let Some(mut client) = fuse_mount_client(&mountpoint.display().to_string()).await? else {
            return Ok(None);
        };
        let response = send_fuse_request(
            &mut client,
            crate::ipc_protocol::IpcRequest::Status {
                mountpoint: mountpoint.display().to_string(),
            },
        )
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

    #[cfg(not(unix))]
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

#[cfg(any(unix, test))]
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

#[cfg(unix)]
async fn refresh_fuse_list_entries(entries: &mut Vec<MountControlListEntry>) {
    let Ok(Some(mut client)) = coordinator_client().await else {
        return;
    };
    let Ok(response) = send_fuse_request(&mut client, crate::ipc_protocol::IpcRequest::List).await
    else {
        return;
    };
    let Some(mounts) = response.mounts else {
        return;
    };
    for mount in mounts {
        let status = send_fuse_request(
            &mut client,
            crate::ipc_protocol::IpcRequest::Status {
                mountpoint: mount.mountpoint.clone(),
            },
        )
        .await
        .ok()
        .and_then(|response| response.status);
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

    #[cfg(unix)]
    {
        let mountpoint = normalize_mountpoint(path);
        let Some(mut client) = fuse_mount_client(&mountpoint.display().to_string()).await? else {
            return Ok(None);
        };
        let response = send_fuse_request(
            &mut client,
            crate::ipc_protocol::IpcRequest::Refresh {
                mountpoint: mountpoint.display().to_string(),
            },
        )
        .await?;
        return Ok(Some(MountControlUpdate {
            backend: MountControlBackend::Fuse,
            mountpoint,
            head_oid: response.head_oid,
            head_ref: None,
        }));
    }

    #[cfg(not(unix))]
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

    #[cfg(unix)]
    {
        let mountpoint = normalize_mountpoint(path);
        let Some(mut client) = fuse_mount_client(&mountpoint.display().to_string()).await? else {
            return Ok(None);
        };
        let response = send_fuse_request(
            &mut client,
            crate::ipc_protocol::IpcRequest::SwitchRef {
                mountpoint: mountpoint.display().to_string(),
                r#ref: git_ref.to_owned(),
            },
        )
        .await?;
        return Ok(Some(MountControlUpdate {
            backend: MountControlBackend::Fuse,
            mountpoint,
            head_oid: response.head_oid,
            head_ref: Some(git_ref.to_owned()),
        }));
    }

    #[cfg(not(unix))]
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

    #[cfg(not(unix))]
    {
        let _ = (path, message, push);
        Ok(None)
    }

    #[cfg(unix)]
    {
        let mountpoint = normalize_mountpoint(path);
        let Some(mut client) = fuse_mount_client(&mountpoint.display().to_string()).await? else {
            return Ok(None);
        };
        let response = send_fuse_request_with_timeout(
            &mut client,
            crate::ipc_protocol::IpcRequest::CommitOverlay {
                mountpoint: mountpoint.display().to_string(),
                message: message.to_owned(),
                push,
            },
            std::time::Duration::from_mins(30),
        )
        .await?;
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

    #[cfg(unix)]
    {
        let mountpoint = normalize_mountpoint(path);
        let Some(mut client) = fuse_mount_client(&mountpoint.display().to_string()).await? else {
            return Ok(None);
        };
        let response = send_fuse_request(
            &mut client,
            crate::ipc_protocol::IpcRequest::ResetOverlay {
                mountpoint: mountpoint.display().to_string(),
            },
        )
        .await?;
        let diff = response
            .overlay_diff
            .ok_or_else(|| CrabError::Internal("FUSE reset response missing diff".into()))?;
        return Ok(Some(diff));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
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

    #[cfg(unix)]
    {
        let mountpoint = normalize_mountpoint(path);
        let Some(mut client) = fuse_mount_client(&mountpoint.display().to_string()).await? else {
            return Ok(None);
        };
        let mountpoint_str = mountpoint.display().to_string();
        let _response = send_fuse_request_with_timeout(
            &mut client,
            crate::ipc_protocol::IpcRequest::Unmount {
                mountpoint: mountpoint_str.clone(),
            },
            std::time::Duration::from_mins(2),
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

    #[cfg(not(unix))]
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

#[cfg(unix)]
async fn coordinator_client() -> Result<Option<crate::ipc_client::IpcClient>> {
    let socket_path = crate::ipc_client::default_socket_path()?;
    match crate::ipc_client::IpcClient::connect(&socket_path).await {
        Ok(client) => Ok(Some(client)),
        Err(error) if error.is_unavailable() => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
async fn fuse_mount_client(mountpoint: &str) -> Result<Option<crate::ipc_client::IpcClient>> {
    let Some(mut client) = coordinator_client().await? else {
        return Ok(None);
    };
    // List establishes ownership without interpreting application error text.
    // A failed exchange must not redirect a live mutation to offline state.
    let response = send_fuse_request(&mut client, crate::ipc_protocol::IpcRequest::List).await?;
    let mounts = response
        .mounts
        .ok_or_else(|| CrabError::Internal("FUSE list response missing mounts".into()))?;
    Ok(mounts
        .iter()
        .any(|mount| mount.mountpoint == mountpoint)
        .then_some(client))
}

#[cfg(unix)]
async fn send_fuse_request(
    client: &mut crate::ipc_client::IpcClient,
    request: crate::ipc_protocol::IpcRequest,
) -> Result<crate::ipc_protocol::IpcResponse> {
    send_fuse_request_with_timeout(client, request, std::time::Duration::from_secs(30)).await
}

#[cfg(unix)]
async fn send_fuse_request_with_timeout(
    client: &mut crate::ipc_client::IpcClient,
    request: crate::ipc_protocol::IpcRequest,
    timeout: std::time::Duration,
) -> Result<crate::ipc_protocol::IpcResponse> {
    let response = client.send_with_timeout(&request, timeout).await?;
    if response.ok {
        return Ok(response);
    }
    Err(CrabError::Internal(response.error.unwrap_or_else(|| {
        "mount control request failed".to_owned()
    })))
}

#[cfg(test)]
mod tests;
