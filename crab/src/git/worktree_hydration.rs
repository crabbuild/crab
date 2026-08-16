//! Per-worktree hydration policy state.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::error::{CrabError, Result};
use crate::git::worktree::WorktreeContext;

pub const HYDRATION_POLICY_FILENAME: &str = "hydration-policy.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorktreeHydrationPolicySource {
    Explicit,
    CloneDefaults,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorktreeHydrationMode {
    Lazy,
    PointerOnly,
    Full,
    Selective,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum WorktreeHydrationSelector {
    CloneDefaults,
    All,
    Patterns {
        include: Vec<String>,
        exclude: Vec<String>,
    },
    Manifest {
        path: String,
        exclude: Vec<String>,
    },
    ManifestRef {
        spec: String,
        exclude: Vec<String>,
    },
    Profile {
        name: String,
        exclude: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorktreeHydrationPolicyStatus {
    Pending,
    Applied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeHydrationPolicyFile {
    pub version: u8,
    pub source: WorktreeHydrationPolicySource,
    pub status: WorktreeHydrationPolicyStatus,
    pub mode: WorktreeHydrationMode,
    pub checkout_suppressed: bool,
    pub prefetch: bool,
    pub selector: WorktreeHydrationSelector,
}

impl WorktreeHydrationPolicyFile {
    pub fn path_for_context(ctx: &WorktreeContext) -> PathBuf {
        ctx.per_worktree_crab_dir.join(HYDRATION_POLICY_FILENAME)
    }

    pub fn read_for_worktree_root(root: &Path) -> Result<Option<Self>> {
        let Ok(ctx) = WorktreeContext::resolve_from_path(root) else {
            return Ok(None);
        };
        Self::read_for_context(&ctx)
    }

    pub fn read_for_context(ctx: &WorktreeContext) -> Result<Option<Self>> {
        let path = Self::path_for_context(ctx);
        if !path.is_file() {
            return Ok(None);
        }
        let body = fs::read_to_string(&path).map_err(CrabError::Io)?;
        toml::from_str(&body)
            .map(Some)
            .map_err(|e| CrabError::Configuration {
                key: path.display().to_string(),
                origin: format!("failed to parse worktree hydration policy: {e}"),
            })
    }

    pub fn write_for_context(&self, ctx: &WorktreeContext) -> Result<()> {
        fs::create_dir_all(&ctx.per_worktree_crab_dir).map_err(CrabError::Io)?;
        let body = toml::to_string_pretty(self).map_err(|e| CrabError::Configuration {
            key: HYDRATION_POLICY_FILENAME.to_owned(),
            origin: format!("failed to serialize worktree hydration policy: {e}"),
        })?;
        fs::write(Self::path_for_context(ctx), body).map_err(CrabError::Io)
    }
}
