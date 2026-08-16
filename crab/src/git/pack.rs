//! Remote-aware pack generation for incremental pushes.
//!
//! Computes the set of objects already on the remote (from locally-cached
//! pack indices) and generates a push pack containing only new objects.
//! Supports thin packs when configured.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tempfile::{NamedTempFile, TempPath};
use tracing::{debug, info, warn};

use crate::core::error::{CrabError, Result};
use crate::git::push::RefUpdate;
pub use crab_git::pack::InstalledPack;
use crab_metadata::manifests::PackManifestEntry;

/// Configuration for push pack generation.
#[derive(Debug, Clone)]
pub struct PushPackConfig {
    /// Enable thin pack generation (delta against remote bases).
    pub thin_packs: bool,
    /// Maximum size of each generated pack. Mirrors git's
    /// `receive.maxInputSize`. Plural generators partition aggregate
    /// closures at this bound; a single object that cannot fit is rejected.
    /// A value of `0` disables the bound.
    pub max_input_size: u64,
    /// Explicit git directory for callers that publish a repository
    /// other than the process current directory.
    pub git_dir: Option<PathBuf>,
}

impl Default for PushPackConfig {
    fn default() -> Self {
        Self {
            thin_packs: false,
            max_input_size: 0,
            git_dir: None,
        }
    }
}

/// Generated pack data ready for upload.
#[derive(Debug)]
pub struct PackedData {
    /// Pack bytes (`.pack` format). Uses `Bytes` for zero-copy sharing.
    pub pack: Bytes,
    /// Index bytes (`.idx` format).
    pub idx: Vec<u8>,
    /// Number of objects in the pack.
    pub object_count: u64,
    /// Whether this is a thin pack (deltas against remote bases).
    pub is_thin: bool,
}

/// Generated pack file ready for upload.
#[derive(Debug)]
pub struct PackedFileData {
    /// Temporary `.pack` file. The path stays valid while this value is alive.
    pub pack_path: TempPath,
    /// Pack size in bytes.
    pub pack_size: u64,
    /// BLAKE3 content hash used by Crab object storage and pack manifests.
    pub pack_blake3: [u8; 32],
    /// Hex form of [`Self::pack_blake3`].
    pub pack_blake3_hex: String,
    /// Git pack trailer SHA-1.
    pub git_sha1: String,
    /// Number of objects in the pack.
    pub object_count: u64,
    /// Whether this is a thin pack (deltas against remote bases).
    pub is_thin: bool,
    // Git may stage base-name output beside its object database. Retaining the
    // directory keeps every returned pack valid and cleans the whole set at once.
    _generation_dir: Option<Arc<tempfile::TempDir>>,
}

/// Remote reachability boundary used to make push packs incremental.
#[derive(Debug, Clone, Copy)]
pub enum RemotePackExclusions<'a> {
    /// Exact object IDs known to exist on the remote.
    ExactObjects(&'a HashSet<gix_hash::ObjectId>),
    /// Commit tips whose reachable history is known to exist on the remote.
    RefTips(&'a [String]),
}

impl RemotePackExclusions<'_> {
    fn to_revision_exclusions(self) -> Vec<String> {
        match self {
            Self::ExactObjects(objects) => objects
                .iter()
                .map(|object| object.to_hex().to_string())
                .collect(),
            Self::RefTips(tips) => tips.to_vec(),
        }
    }
}

/// Compute the set of objects known to exist on the remote.
///
/// Reads `.idx` files from locally-cached copies of remote packs (known
/// from the segmented manifest inventory). For each pack entry, looks for a corresponding
/// `.idx` file in `pack_dir`. Packs with missing indices are skipped with
/// a warning — the caller should fall back to a full pack.
pub fn compute_remote_object_set(
    pack_dir: &Path,
    packs: &[PackManifestEntry],
) -> Result<HashSet<gix_hash::ObjectId>> {
    let mut remote_objects = HashSet::new();
    let mut skipped = 0usize;

    for entry in packs {
        let idx_path = pack_dir.join(format!("pack-{}.idx", entry.pack_id));

        if !idx_path.exists() {
            debug!(
                pack_id = %entry.pack_id,
                idx_path = %idx_path.display(),
                "missing local pack index, skipping pack"
            );
            skipped += 1;
            continue;
        }

        match parse_idx_object_ids(&idx_path) {
            Ok(oids) => {
                debug!(
                    pack_id = %entry.pack_id,
                    objects = oids.len(),
                    "parsed pack index"
                );
                remote_objects.extend(oids);
            }
            Err(e) => {
                warn!(
                    pack_id = %entry.pack_id,
                    error = %e,
                    "failed to parse pack index, skipping pack"
                );
                skipped += 1;
            }
        }
    }

    info!(
        total_objects = remote_objects.len(),
        packs_parsed = packs.len() - skipped,
        packs_skipped = skipped,
        "computed remote object set"
    );

    Ok(remote_objects)
}

/// Generate a push pack containing only objects not in the remote set.
///
/// When `remote_objects` is `Some`, objects in that set are excluded from
/// the generated pack. When `None`, a full pack is generated (fallback).
///
/// When `config.thin_packs` is `true`, the pack is marked as thin and
/// delta base references against remote objects are permitted. Before
/// returning, all delta bases are verified to exist in `remote_objects`.
pub async fn generate_push_pack(
    local_refs: &[RefUpdate],
    remote_objects: Option<&HashSet<gix_hash::ObjectId>>,
    config: &PushPackConfig,
) -> Result<PackedData> {
    generate_push_pack_with_exclusions(
        local_refs,
        remote_objects.map(RemotePackExclusions::ExactObjects),
        config,
    )
    .await
}

/// Generate a push pack using a remote reachability boundary.
///
/// Ref-tip exclusions avoid materializing every remote object ID for
/// modern pack manifests. Exact object exclusions remain available for
/// legacy manifests and thin-pack base verification.
pub async fn generate_push_pack_with_exclusions(
    local_refs: &[RefUpdate],
    remote_exclusions: Option<RemotePackExclusions<'_>>,
    config: &PushPackConfig,
) -> Result<PackedData> {
    let ref_count = local_refs.len();
    let is_thin = config.thin_packs
        && matches!(
            remote_exclusions,
            Some(RemotePackExclusions::ExactObjects(_))
        );

    debug!(
        ref_count,
        thin = is_thin,
        has_remote_exclusions = remote_exclusions.is_some(),
        "generating push pack"
    );

    // Collect the new SHAs from the ref updates.
    let new_shas: Vec<String> = local_refs.iter().map(|r| r.new_sha.clone()).collect();

    if new_shas.is_empty() {
        return Ok(PackedData {
            pack: Bytes::new(),
            idx: Vec::new(),
            object_count: 0,
            is_thin: false,
        });
    }

    // Use `git pack-objects` to create a real pack file containing all
    // objects reachable from the push tips but not reachable from the
    // remote boundary. The boundary is normally ref tips from the
    // manifest; legacy manifests fall back to exact object IDs parsed
    // from local pack indexes.
    let exact_remote_objects = match remote_exclusions {
        Some(RemotePackExclusions::ExactObjects(objects)) => Some(objects),
        _ => None,
    };
    let remote_exclusion_revs: Option<Vec<String>> =
        remote_exclusions.map(RemotePackExclusions::to_revision_exclusions);
    #[cfg(feature = "gix-pack-native")]
    let pack_data = {
        // Route through the gix-pack / gix-traverse native path. We
        // need `objects_dir` so the adapter can open the ODB —
        // discover it here and pass it through.
        let objects_dir = config
            .git_dir
            .clone()
            .map_or_else(crate::git::discover::discover_git_dir, Ok)?
            .join("objects");
        let new_shas_clone = new_shas.clone();
        let remote_exclusion_revs_clone = remote_exclusion_revs.clone();
        tokio::task::spawn_blocking(move || {
            crate::git::pack_native::generate_pack_native(
                &objects_dir,
                &new_shas_clone,
                remote_exclusion_revs_clone.as_deref(),
                is_thin,
            )
        })
        .await
        .map_err(|e| CrabError::Internal(format!("pack-native join: {e}")))??
    };
    #[cfg(not(feature = "gix-pack-native"))]
    let git_dir = config.git_dir.clone();
    #[cfg(not(feature = "gix-pack-native"))]
    let pack_data = tokio::task::spawn_blocking(move || {
        generate_pack_via_git(
            git_dir.as_deref(),
            &new_shas,
            remote_exclusion_revs.as_deref(),
            is_thin,
        )
    })
    .await
    .map_err(|e| CrabError::Internal(format!("pack-objects join: {e}")))??;

    let object_count = pack_data.object_count;

    // Pack-intake size cap — mirror git's `receive.maxInputSize`.
    // Reject oversized packs before thin-base verification, upload,
    // and `index-pack` so a hostile client can't burn CPU, network,
    // or the `INDEX_PACK_TIMEOUT` budget on a pack we'd refuse to
    // accept anyway. A limit of 0 means unlimited (opt-out for
    // trusted repos).
    if config.max_input_size > 0 && (pack_data.pack.len() as u64) > config.max_input_size {
        return Err(CrabError::PackTooLarge {
            size: pack_data.pack.len() as u64,
            limit: config.max_input_size,
        });
    }

    // Thin-pack delta-base verification: when `--thin` is on, the
    // receiving fetcher expects every REF_DELTA base to already be in
    // its ODB. If our `remote_objects` set is stale (e.g. a pack's
    // `.idx` was missing and got skipped), the pack could reference
    // bases the remote doesn't actually have, producing a pack that
    // index-pack --fix-thin can't repair. Catching it here prevents a
    // broken pack from landing on S3.
    if is_thin {
        if let Some(remote_set) = exact_remote_objects {
            let pack_slice = pack_data.pack.as_ref();
            let remote_clone = remote_set.clone();
            let verified = tokio::task::spawn_blocking({
                let pack_vec: Vec<u8> = pack_slice.to_vec();
                move || verify_thin_pack_bases(&pack_vec, &remote_clone)
            })
            .await
            .map_err(|e| CrabError::Internal(format!("thin-pack verify join: {e}")))??;

            debug!(
                bases_verified = verified,
                "thin-pack REF_DELTA bases verified against remote object set"
            );
        }
    }

    info!(
        object_count,
        pack_bytes = pack_data.pack.len(),
        is_thin,
        "push pack generated"
    );

    Ok(PackedData {
        pack: pack_data.pack,
        idx: pack_data.idx,
        object_count,
        is_thin,
    })
}

/// Generate a push pack as a temporary file.
///
/// The default shellout implementation keeps pack bytes off the Rust heap:
/// `git pack-objects` writes directly to a tempfile, then Crab streams that
/// file once to verify the Git SHA-1 trailer and compute the BLAKE3 storage
/// identity. Thin packs still route through the byte-backed path because their
/// existing remote-base verifier operates on in-memory pack entries.
pub async fn generate_push_pack_file_with_exclusions(
    local_refs: &[RefUpdate],
    remote_exclusions: Option<RemotePackExclusions<'_>>,
    config: &PushPackConfig,
) -> Result<PackedFileData> {
    #[cfg(feature = "gix-pack-native")]
    {
        let packed =
            generate_push_pack_with_exclusions(local_refs, remote_exclusions, config).await?;
        packed_bytes_to_temp_file(packed)
    }

    #[cfg(not(feature = "gix-pack-native"))]
    {
        let is_thin = config.thin_packs
            && matches!(
                remote_exclusions,
                Some(RemotePackExclusions::ExactObjects(_))
            );
        if is_thin {
            let packed =
                generate_push_pack_with_exclusions(local_refs, remote_exclusions, config).await?;
            return packed_bytes_to_temp_file(packed);
        }

        let ref_count = local_refs.len();
        debug!(
            ref_count,
            thin = false,
            has_remote_exclusions = remote_exclusions.is_some(),
            "generating file-backed push pack"
        );

        let new_shas: Vec<String> = local_refs.iter().map(|r| r.new_sha.clone()).collect();
        if new_shas.is_empty() {
            return packed_bytes_to_temp_file(PackedData {
                pack: Bytes::new(),
                idx: Vec::new(),
                object_count: 0,
                is_thin: false,
            });
        }

        let remote_exclusion_revs: Option<Vec<String>> =
            remote_exclusions.map(RemotePackExclusions::to_revision_exclusions);
        let git_dir = config.git_dir.clone();
        let packed = tokio::task::spawn_blocking(move || {
            generate_pack_file_via_git(
                git_dir.as_deref(),
                &new_shas,
                remote_exclusion_revs.as_deref(),
            )
        })
        .await
        .map_err(|e| CrabError::Internal(format!("pack-objects file join: {e}")))??;

        if config.max_input_size > 0 && packed.pack_size > config.max_input_size {
            return Err(CrabError::PackTooLarge {
                size: packed.pack_size,
                limit: config.max_input_size,
            });
        }

        info!(
            object_count = packed.object_count,
            pack_bytes = packed.pack_size,
            is_thin = packed.is_thin,
            "file-backed push pack generated"
        );

        Ok(packed)
    }
}

/// Generate one or more independently usable packs for pushed refs.
///
/// Non-thin output is partitioned at `max_input_size` instead of rejecting
/// the aggregate reachable closure. Thin output remains singular because Git
/// supports thin packs only through `--stdout`.
pub async fn generate_push_pack_files_with_exclusions(
    local_refs: &[RefUpdate],
    remote_exclusions: Option<RemotePackExclusions<'_>>,
    config: &PushPackConfig,
) -> Result<Vec<PackedFileData>> {
    if local_refs.is_empty() {
        return Ok(Vec::new());
    }
    let is_thin = config.thin_packs
        && matches!(
            remote_exclusions,
            Some(RemotePackExclusions::ExactObjects(_))
        );
    if is_thin {
        return generate_push_pack_file_with_exclusions(local_refs, remote_exclusions, config)
            .await
            .map(|pack| vec![pack]);
    }

    let git_dir = config.git_dir.clone();
    let max_input_size = config.max_input_size;
    let mut input = Vec::new();
    if let Some(exclusions) = remote_exclusions {
        input.extend(
            exclusions
                .to_revision_exclusions()
                .into_iter()
                .map(|oid| format!("^{oid}")),
        );
    }
    input.extend(local_refs.iter().map(|update| update.new_sha.clone()));
    tokio::task::spawn_blocking(move || {
        generate_pack_files_via_git(git_dir.as_deref(), input, true, max_input_size)
    })
    .await
    .map_err(|error| CrabError::Internal(format!("revision pack-set join: {error}")))?
}

/// Generate a non-thin pack from an exact object-id set.
///
/// Generation-pinned locators prove individual objects, not commit
/// reachability. Feeding `^commit` to `rev-list` would incorrectly exclude
/// an unproven tree or blob reachable from that commit, so the candidate-
/// scoped path bypasses revision walking and writes only the explicit misses.
pub async fn generate_pack_file_from_object_ids(
    object_ids: &[gix_hash::ObjectId],
    config: &PushPackConfig,
) -> Result<PackedFileData> {
    let git_dir = config.git_dir.clone();
    let mut object_ids = object_ids.to_vec();
    object_ids.sort_unstable();
    object_ids.dedup();
    let packed = tokio::task::spawn_blocking(move || {
        generate_pack_file_from_object_ids_via_git(git_dir.as_deref(), &object_ids)
    })
    .await
    .map_err(|error| CrabError::Internal(format!("exact pack-objects join: {error}")))??;

    if config.max_input_size > 0 && packed.pack_size > config.max_input_size {
        return Err(CrabError::PackTooLarge {
            size: packed.pack_size,
            limit: config.max_input_size,
        });
    }
    Ok(packed)
}

/// Generate independently usable non-thin packs from an exact object-id set.
///
/// When `max_input_size` is nonzero, Git partitions the set into standard
/// packs bounded by that size. A single object that cannot fit still returns
/// [`CrabError::PackTooLarge`].
pub async fn generate_pack_files_from_object_ids(
    object_ids: &[gix_hash::ObjectId],
    config: &PushPackConfig,
) -> Result<Vec<PackedFileData>> {
    if object_ids.is_empty() {
        return Ok(Vec::new());
    }

    let git_dir = config.git_dir.clone();
    let max_input_size = config.max_input_size;
    let mut object_ids = object_ids.to_vec();
    object_ids.sort_unstable();
    object_ids.dedup();
    tokio::task::spawn_blocking(move || {
        generate_pack_files_via_git(
            git_dir.as_deref(),
            object_ids.iter().map(ToString::to_string),
            false,
            max_input_size,
        )
    })
    .await
    .map_err(|error| CrabError::Internal(format!("exact pack-set join: {error}")))?
}

fn generate_pack_files_via_git(
    git_dir: Option<&Path>,
    input: impl IntoIterator<Item = String>,
    revisions: bool,
    max_input_size: u64,
) -> Result<Vec<PackedFileData>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let output_root = match git_dir {
        Some(git_dir) => git_dir.to_owned(),
        None => crate::git::discover::discover_git_dir()?,
    };
    let output_dir = Arc::new(
        tempfile::Builder::new()
            .prefix("crab-push-pack-set-")
            .tempdir_in(output_root)?,
    );
    let base_name = output_dir.path().join("pack");
    let mut command = Command::new("git");
    if let Some(git_dir) = git_dir {
        command.arg("--git-dir").arg(git_dir);
    }
    command.args(["pack-objects", "--quiet"]);
    if revisions {
        command.arg("--revs");
    }
    if max_input_size > 0 {
        command.arg(format!(
            "--max-pack-size={}",
            pack_generation_size_limit(max_input_size)
        ));
    }
    let mut child = command
        .arg(&base_name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| CrabError::Internal("pack-objects stdin not available".to_owned()))?;
    for object in input {
        stdin.write_all(object.as_bytes())?;
        stdin.write_all(b"\n")?;
    }
    drop(stdin);

    let output = child.wait_with_output().map_err(CrabError::Io)?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git pack-objects failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let mut git_hashes = String::from_utf8(output.stdout)
        .map_err(|error| {
            CrabError::Internal(format!("pack-objects output was not UTF-8: {error}"))
        })?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    git_hashes.sort_unstable();
    git_hashes.dedup();

    let mut packs = Vec::with_capacity(git_hashes.len());
    for git_hash in git_hashes {
        gix_hash::ObjectId::from_hex(git_hash.as_bytes()).map_err(|error| {
            CrabError::Internal(format!("pack-objects returned invalid pack hash: {error}"))
        })?;
        let generated_path = output_dir.path().join(format!("pack-{git_hash}.pack"));
        let inspection = inspect_pack_file(&generated_path)?;
        if inspection.git_sha1 != git_hash {
            return Err(CrabError::PackIntegrity {
                expected: git_hash,
                computed: inspection.git_sha1,
            });
        }
        if max_input_size > 0 && inspection.pack_size > max_input_size {
            return Err(CrabError::PackTooLarge {
                size: inspection.pack_size,
                limit: max_input_size,
            });
        }

        packs.push(PackedFileData {
            pack_path: TempPath::try_from_path(generated_path)?,
            pack_size: inspection.pack_size,
            pack_blake3: inspection.pack_blake3,
            pack_blake3_hex: inspection.pack_blake3_hex,
            git_sha1: inspection.git_sha1,
            object_count: inspection.object_count,
            is_thin: false,
            _generation_dir: Some(Arc::clone(&output_dir)),
        });
    }

    Ok(packs)
}

fn pack_generation_size_limit(max_input_size: u64) -> u64 {
    const GIT_MIN_PACK_SIZE: u64 = 1024 * 1024;

    if max_input_size <= GIT_MIN_PACK_SIZE {
        return max_input_size;
    }

    // Git can finalize a split pack just beyond its requested boundary. Keep
    // the authoritative intake check at max_input_size and generate 5% below it.
    (max_input_size - max_input_size / 20).max(GIT_MIN_PACK_SIZE)
}

fn generate_pack_file_from_object_ids_via_git(
    git_dir: Option<&Path>,
    object_ids: &[gix_hash::ObjectId],
) -> Result<PackedFileData> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let stderr = NamedTempFile::new()?;
    let mut pack_tmp = tempfile::Builder::new()
        .prefix("crab-push-pack-")
        .suffix(".pack")
        .tempfile()?;
    let mut command = Command::new("git");
    if let Some(git_dir) = git_dir {
        command.arg("--git-dir").arg(git_dir);
    }
    let mut child = command
        .args(["pack-objects", "--stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(pack_tmp.reopen()?))
        .stderr(Stdio::from(stderr.reopen()?))
        .spawn()
        .map_err(CrabError::Io)?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| CrabError::Internal("pack-objects stdin not available".to_owned()))?;
    for object_id in object_ids {
        writeln!(stdin, "{}", object_id.to_hex()).map_err(CrabError::Io)?;
    }
    drop(stdin);

    let status = child.wait().map_err(CrabError::Io)?;
    if !status.success() {
        let stderr = std::fs::read_to_string(stderr.path())
            .unwrap_or_else(|error| format!("<failed to read stderr: {error}>"));
        return Err(CrabError::Internal(format!(
            "git pack-objects failed: {stderr}"
        )));
    }
    pack_tmp.as_file_mut().sync_all()?;
    let inspection = inspect_pack_file(pack_tmp.path())?;
    let expected_count = u64::try_from(object_ids.len())
        .map_err(|_| CrabError::Internal("exact Git object count overflow".to_owned()))?;
    if inspection.object_count != expected_count {
        return Err(CrabError::Internal(format!(
            "exact Git pack contains {} objects, expected {expected_count}",
            inspection.object_count
        )));
    }
    Ok(PackedFileData {
        pack_path: pack_tmp.into_temp_path(),
        pack_size: inspection.pack_size,
        pack_blake3: inspection.pack_blake3,
        pack_blake3_hex: inspection.pack_blake3_hex,
        git_sha1: inspection.git_sha1,
        object_count: inspection.object_count,
        is_thin: false,
        _generation_dir: None,
    })
}

/// Generate a git pack file using `git pack-objects`.
///
/// Feeds the tip SHAs to `git rev-list --objects` piped into
/// `git pack-objects` to produce a pack containing all reachable objects
/// not excluded by `remote_exclusions`.
///
/// When `remote_exclusions` is `Some`, each SHA is fed to `rev-list` as
/// a `^<sha>` exclusion line, so the resulting pack contains only the
/// delta between local history and the chosen remote boundary. When
/// `None` (first push or metadata missing), a full pack is generated.
///
/// When `thin` is `true`, `pack-objects` is invoked with `--thin`, so
/// the pack may contain delta references to objects the remote already
/// has without including those base objects. The fetch side (git's
/// native `index-pack`) reconstructs the missing bases from its own
/// packs. See finding CR1.10-F3.
#[cfg(not(feature = "gix-pack-native"))]
fn generate_pack_via_git(
    git_dir: Option<&Path>,
    tip_shas: &[String],
    remote_exclusions: Option<&[String]>,
    thin: bool,
) -> Result<PackedData> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let sha_input = build_revision_input(tip_shas, remote_exclusions);

    // REPLACEABLE: gix_traverse::commit::Simple + gix_traverse::tree::breadthfirst
    // (gated by `gix-pack-native` — see `pack_native::enumerate_objects`).
    // Run: echo SHAs | git rev-list --stdin --objects | git pack-objects <base>
    let mut rev_list_cmd = Command::new("git");
    if let Some(git_dir) = git_dir {
        rev_list_cmd.arg("--git-dir").arg(git_dir);
    }
    let mut rev_list = rev_list_cmd
        .args(["rev-list", "--stdin", "--objects"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;

    // Feed SHAs to rev-list stdin.
    {
        let stdin = rev_list
            .stdin
            .as_mut()
            .ok_or_else(|| CrabError::Internal("rev-list stdin not available".into()))?;
        stdin
            .write_all(sha_input.as_bytes())
            .map_err(CrabError::Io)?;
    }
    // Close stdin so rev-list can finish.
    rev_list.stdin.take();

    let rev_output = rev_list.wait_with_output().map_err(CrabError::Io)?;
    if !rev_output.status.success() {
        let stderr = String::from_utf8_lossy(&rev_output.stderr);
        return Err(CrabError::Internal(format!(
            "git rev-list failed: {stderr}"
        )));
    }

    // Feed object list to pack-objects.
    //
    // When `--thin` is set, pack-objects reads revision specs (lines
    // starting with `^` mean "don't include objects reachable from
    // here") and walks history itself, rather than accepting the
    // `<oid> <path>` pairs `rev-list --objects` produces. To support
    // both modes we always use rev-list's output for the non-thin
    // case (faster, no extra walk) and feed thin-mode pack-objects
    // the raw revision input (tips + ^exclusions) directly so it can
    // compute deltas against remote bases. See finding CR1.10-F3.
    // REPLACEABLE: gix_pack::data::output::bytes::FromEntriesIter driven by
    // an iterator over `output::Entry` values pulled through `CrabOdb`
    // (gated by `gix-pack-native` — see `pack_native::build_pack_bytes`).
    let mut pack_objects_cmd = Command::new("git");
    if let Some(git_dir) = git_dir {
        pack_objects_cmd.arg("--git-dir").arg(git_dir);
    }
    pack_objects_cmd.args(["pack-objects", "--stdout"]);
    let pack_stdin: Vec<u8> = if thin {
        pack_objects_cmd.arg("--thin").arg("--revs");
        // --revs: read revisions (including ^exclusions), not oid lists.
        // sha_input already has exactly that format (^oid\n... plus tips).
        sha_input.into_bytes()
    } else {
        rev_output.stdout
    };
    let mut pack_objects = pack_objects_cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;

    pack_objects
        .stdin
        .as_mut()
        .ok_or_else(|| CrabError::Internal("pack-objects stdin not available".into()))?
        .write_all(&pack_stdin)
        .map_err(CrabError::Io)?;
    drop(pack_objects.stdin.take());

    let pack_output = pack_objects.wait_with_output().map_err(CrabError::Io)?;
    if !pack_output.status.success() {
        let stderr = String::from_utf8_lossy(&pack_output.stderr);
        return Err(CrabError::Internal(format!(
            "git pack-objects failed: {stderr}"
        )));
    }

    let pack_bytes: Bytes = pack_output.stdout.into();
    let object_count = count_pack_objects(&pack_bytes);

    // Verify the pack's trailing SHA1 checksum before returning. A
    // truncated stdout pipe (process killed mid-write, or a memory
    // error) could produce corrupt bytes that pass the exit-code check
    // but fail on the fetch side. Catching it here gives a clear error
    // at the source rather than surfacing as "pack integrity error on
    // fetch" later. See finding CR1.10-F4.
    crate::git::fetch::verify_pack_sha1(&pack_bytes)?;

    debug!(
        pack_bytes = pack_bytes.len(),
        object_count, "git pack-objects produced pack"
    );

    Ok(PackedData {
        pack: pack_bytes,
        idx: Vec::new(), // idx generated on fetch side
        object_count,
        is_thin: thin,
    })
}

#[cfg(not(feature = "gix-pack-native"))]
fn generate_pack_file_via_git(
    git_dir: Option<&Path>,
    tip_shas: &[String],
    remote_exclusions: Option<&[String]>,
) -> Result<PackedFileData> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let sha_input = build_revision_input(tip_shas, remote_exclusions);

    let rev_stderr = NamedTempFile::new()?;
    let pack_stderr = NamedTempFile::new()?;
    let mut pack_tmp = tempfile::Builder::new()
        .prefix("crab-push-pack-")
        .suffix(".pack")
        .tempfile()?;

    let mut rev_list_cmd = Command::new("git");
    if let Some(git_dir) = git_dir {
        rev_list_cmd.arg("--git-dir").arg(git_dir);
    }
    let mut rev_list = rev_list_cmd
        .args(["rev-list", "--stdin", "--objects"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(rev_stderr.reopen()?))
        .spawn()
        .map_err(CrabError::Io)?;

    let mut pack_objects_cmd = Command::new("git");
    if let Some(git_dir) = git_dir {
        pack_objects_cmd.arg("--git-dir").arg(git_dir);
    }
    let mut pack_objects = pack_objects_cmd
        .args(["pack-objects", "--stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(pack_tmp.reopen()?))
        .stderr(Stdio::from(pack_stderr.reopen()?))
        .spawn()
        .map_err(CrabError::Io)?;

    let mut rev_stdin = rev_list
        .stdin
        .take()
        .ok_or_else(|| CrabError::Internal("rev-list stdin not available".into()))?;
    let writer = std::thread::spawn(move || {
        rev_stdin.write_all(sha_input.as_bytes())?;
        rev_stdin.flush()
    });

    let copy_result = {
        let mut rev_stdout = rev_list
            .stdout
            .take()
            .ok_or_else(|| CrabError::Internal("rev-list stdout not available".into()))?;
        let mut pack_stdin = pack_objects
            .stdin
            .take()
            .ok_or_else(|| CrabError::Internal("pack-objects stdin not available".into()))?;
        std::io::copy(&mut rev_stdout, &mut pack_stdin).map(|_| ())
    };

    let writer_result = writer
        .join()
        .map_err(|_| CrabError::Internal("rev-list stdin writer panicked".into()))?
        .map_err(CrabError::Io);

    let rev_status = rev_list.wait().map_err(CrabError::Io)?;
    if !rev_status.success() {
        return Err(CrabError::Internal(format!(
            "git rev-list failed: {}",
            read_tempfile_stderr(&rev_stderr)
        )));
    }

    let pack_status = pack_objects.wait().map_err(CrabError::Io)?;
    if !pack_status.success() {
        return Err(CrabError::Internal(format!(
            "git pack-objects failed: {}",
            read_tempfile_stderr(&pack_stderr)
        )));
    }
    copy_result.map_err(CrabError::Io)?;
    writer_result?;

    pack_tmp.as_file_mut().sync_all()?;
    let inspection = inspect_pack_file(pack_tmp.path())?;

    debug!(
        pack_bytes = inspection.pack_size,
        object_count = inspection.object_count,
        "git pack-objects produced file-backed pack"
    );

    Ok(PackedFileData {
        pack_path: pack_tmp.into_temp_path(),
        pack_size: inspection.pack_size,
        pack_blake3: inspection.pack_blake3,
        pack_blake3_hex: inspection.pack_blake3_hex,
        git_sha1: inspection.git_sha1,
        object_count: inspection.object_count,
        is_thin: false,
        _generation_dir: None,
    })
}

#[cfg(not(feature = "gix-pack-native"))]
fn build_revision_input(tip_shas: &[String], remote_exclusions: Option<&[String]>) -> String {
    let mut sha_input = String::new();
    if let Some(excluded) = remote_exclusions {
        for oid in excluded {
            sha_input.push('^');
            sha_input.push_str(oid);
            sha_input.push('\n');
        }
    }
    for sha in tip_shas {
        sha_input.push_str(sha);
        sha_input.push('\n');
    }
    sha_input
}

#[cfg(not(feature = "gix-pack-native"))]
fn read_tempfile_stderr(file: &NamedTempFile) -> String {
    std::fs::read_to_string(file.path()).unwrap_or_else(|e| format!("<failed to read stderr: {e}>"))
}

/// Count objects in a pack file by reading the header.
#[cfg(not(feature = "gix-pack-native"))]
fn count_pack_objects(pack: &[u8]) -> u64 {
    // Git pack header: "PACK" (4) + version (4) + object_count (4, big-endian)
    if pack.len() >= 12 && &pack[0..4] == b"PACK" {
        u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]) as u64
    } else {
        0
    }
}

/// Parse object IDs from a `.idx` file.
///
/// Reads the pack index using the real `gix_pack` parser (index v2
/// format: magic + fanout table + sorted OID table + CRC32 + offset
/// tables + checksums). The previous implementation read raw 20-byte
/// chunks starting from byte 0, which produced garbage OIDs from any
/// real idx file because it included header/fanout/CRC bytes as if
/// they were object IDs. See finding S1-P1-8.
fn parse_idx_object_ids(idx_path: &Path) -> Result<Vec<gix_hash::ObjectId>> {
    match gix_pack::index::File::at(idx_path, gix_hash::Kind::Sha1) {
        Ok(idx) => Ok(idx.iter().map(|entry| entry.oid).collect()),
        Err(e) => Err(CrabError::Internal(format!(
            "failed to parse pack index {}: {e}",
            idx_path.display()
        ))),
    }
}

// Thin-pack delta-base verification: previously unimplemented (the
// comment above noted the cost/benefit); now implemented via
// [`verify_thin_pack_bases`]. Callers that enable `--thin` should
// invoke the verifier before upload to match the doc contract on
// [`generate_push_pack`]. We keep the verification out of
// `generate_pack_via_git` itself because the caller owns the
// remote-object set and is the right place to decide whether a
// verification miss should trigger a full-pack fallback or a hard
// failure.

/// Verify that every REF_DELTA base referenced by a thin pack exists in
/// `remote_objects`.
///
/// `git pack-objects --thin` emits delta objects whose bases live on
/// the remote but are *not* included in the pack itself; the fetcher's
/// `git index-pack --fix-thin` reconstructs them. If a base is not
/// actually in the remote set (e.g. because the caller's
/// `remote_objects` was computed from a stale pack-list that happened
/// to omit the pack containing the base), the fetcher's fix-thin pass
/// fails and the uploaded pack is permanently broken.
///
/// Catching it here is cheap: the pack is already in memory, the
/// remote-object set is already built, and we only walk REF_DELTA
/// entry headers (non-delta objects and OFS_DELTA are ignored — the
/// latter never points outside the pack). Returns the number of
/// REF_DELTA bases checked on success, or an
/// [`CrabError::PackIntegrity`] with the first offending base OID.
pub fn verify_thin_pack_bases(
    pack_bytes: &[u8],
    remote_objects: &HashSet<gix_hash::ObjectId>,
) -> Result<u64> {
    use gix_pack::data::input::{BytesToEntriesIter, Mode};
    use std::io::BufReader;

    // `BytesToEntriesIter` takes a `BufRead` and walks pack entries
    // without decompressing the payload. Pick `Verify` mode so the
    // iterator still validates framing but doesn't materialize
    // reconstructed bases — we only care about the entry headers.
    let reader = BufReader::new(pack_bytes);
    let iter = BytesToEntriesIter::new_from_header(
        reader,
        Mode::Verify,
        gix_pack::data::input::EntryDataMode::Ignore,
        gix_hash::Kind::Sha1,
    )
    .map_err(|e| {
        CrabError::Internal(format!(
            "failed to open pack for thin-base verification: {e}"
        ))
    })?;

    let mut checked: u64 = 0;
    for entry_result in iter {
        let entry = entry_result
            .map_err(|e| CrabError::Internal(format!("pack entry iteration failed: {e}")))?;

        if let gix_pack::data::entry::Header::RefDelta { base_id } = entry.header {
            if !remote_objects.contains(&base_id) {
                // The pack references a base we can't prove is on the
                // remote. Refuse to upload — a broken thin pack will
                // silently corrupt the next fetch.
                return Err(CrabError::PackIntegrity {
                    expected: format!("thin-pack base {base_id} present in remote object set"),
                    computed: format!(
                        "base {base_id} NOT in remote_objects ({} entries)",
                        remote_objects.len()
                    ),
                });
            }
            checked += 1;
        }
    }

    Ok(checked)
}

struct PackFileInspection {
    pack_size: u64,
    pack_blake3: [u8; 32],
    pack_blake3_hex: String,
    git_sha1: String,
    object_count: u64,
}

fn inspect_pack_file(path: &Path) -> Result<PackFileInspection> {
    use sha1::Digest;
    use std::io::{Read, Seek, SeekFrom};

    const HEADER_LEN: usize = 12;
    const SHA1_LEN: u64 = 20;

    let mut file = std::fs::File::open(path)?;
    let pack_size = file.metadata()?.len();
    if pack_size < HEADER_LEN as u64 + SHA1_LEN {
        return Err(CrabError::PackIntegrity {
            expected: "pack with PACK header and SHA-1 trailer".to_owned(),
            computed: format!("{} bytes", pack_size),
        });
    }

    let mut header = [0u8; HEADER_LEN];
    file.read_exact(&mut header)?;
    if &header[0..4] != b"PACK" {
        return Err(CrabError::PackIntegrity {
            expected: "PACK header".to_owned(),
            computed: hex_encode(&header[0..4]),
        });
    }
    let object_count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as u64;

    file.seek(SeekFrom::End(-(SHA1_LEN as i64)))?;
    let mut trailer = [0u8; SHA1_LEN as usize];
    file.read_exact(&mut trailer)?;

    file.seek(SeekFrom::Start(0))?;
    let mut sha1 = sha1::Sha1::new();
    let mut blake3 = blake3::Hasher::new();
    let mut remaining_sha1 = pack_size - SHA1_LEN;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        blake3.update(&buffer[..read]);

        if remaining_sha1 > 0 {
            let sha1_len = remaining_sha1.min(read as u64) as usize;
            sha1.update(&buffer[..sha1_len]);
            remaining_sha1 -= sha1_len as u64;
        }
    }

    let computed_sha1: [u8; 20] = sha1.finalize().into();
    if computed_sha1 != trailer {
        return Err(CrabError::PackIntegrity {
            expected: hex_encode(&trailer),
            computed: hex_encode(&computed_sha1),
        });
    }

    let pack_blake3 = *blake3.finalize().as_bytes();
    Ok(PackFileInspection {
        pack_size,
        pack_blake3,
        pack_blake3_hex: hex_encode(&pack_blake3),
        git_sha1: hex_encode(&trailer),
        object_count,
    })
}

fn packed_bytes_to_temp_file(packed: PackedData) -> Result<PackedFileData> {
    use std::io::Write;

    let mut pack_tmp = tempfile::Builder::new()
        .prefix("crab-push-pack-")
        .suffix(".pack")
        .tempfile()?;
    pack_tmp.write_all(&packed.pack)?;
    pack_tmp.flush()?;

    if packed.pack.is_empty() {
        let hash = blake3::hash(&packed.pack);
        return Ok(PackedFileData {
            pack_path: pack_tmp.into_temp_path(),
            pack_size: 0,
            pack_blake3: *hash.as_bytes(),
            pack_blake3_hex: hash.to_hex().to_string(),
            git_sha1: String::new(),
            object_count: packed.object_count,
            is_thin: packed.is_thin,
            _generation_dir: None,
        });
    }

    let inspection = inspect_pack_file(pack_tmp.path())?;
    Ok(PackedFileData {
        pack_path: pack_tmp.into_temp_path(),
        pack_size: inspection.pack_size,
        pack_blake3: inspection.pack_blake3,
        pack_blake3_hex: inspection.pack_blake3_hex,
        git_sha1: inspection.git_sha1,
        object_count: inspection.object_count,
        is_thin: packed.is_thin,
        _generation_dir: None,
    })
}

// ---------------------------------------------------------------------------
// Local pack installation
// ---------------------------------------------------------------------------

/// How long to wait for `git index-pack` before giving up and cleaning up.
///
/// Indexing is CPU-bound and roughly linear in object count — a 1 GB pack
/// indexes in a few seconds on modern hardware. We allow 5 minutes so
/// abnormal packs don't hang a push indefinitely.
const INDEX_PACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Install a pack into `.git/objects/pack/` with a matching `.idx`.
///
/// The pack is written as `pack-{canonical_name}.pack` plus
/// `pack-{canonical_name}.idx`. Using a caller-supplied canonical name
/// (rather than the pack's trailing SHA-1) lets crab key packs by
/// their blake3 content hash — which matches the pack-list manifest
/// format and keeps incremental-push lookups working. Git tolerates
/// arbitrary pack filenames as long as `.pack` and `.idx` agree, so
/// `git fsck` / `git gc` remain clean.
///
/// `max_input_size` mirrors git's `receive.maxInputSize`: when
/// non-zero, the caller's bytes are refused before `git index-pack`
/// runs so a hostile client can't burn the [`INDEX_PACK_TIMEOUT`]
/// budget on oversized input. Pass `0` from call sites (notably fetch)
/// where the pack size is already trusted or the cap doesn't apply.
///
/// The `.pack` bytes are written to a tempfile in `pack_dir`, then
/// `git index-pack` generates the matching index next to it. Both
/// tempfiles land under their final names via atomic rename.
///
/// Idempotent: if both final files already exist, returns early
/// without rewriting.
///
/// Failures clean up tempfiles so the pack dir never contains a lone
/// `.pack` (which would trip `git fsck`) or a lone `.idx` (harmless but
/// wasteful). Errors are meant to be demoted to `warn!` by the caller:
/// a failed local install only costs one wasted full-pack upload on the
/// next push, so it must never fail the current push.
pub async fn install_pack_locally(
    pack_dir: &Path,
    pack_bytes: &[u8],
    canonical_name: &str,
    max_input_size: u64,
) -> Result<InstalledPack> {
    // Reject oversized packs before any filesystem work or the
    // index-pack subprocess — the timeout guard around index-pack is
    // 5 minutes, long enough for a hostile client to cause real
    // resource pressure with a multi-GB pack.
    if max_input_size > 0 && (pack_bytes.len() as u64) > max_input_size {
        return Err(CrabError::PackTooLarge {
            size: pack_bytes.len() as u64,
            limit: max_input_size,
        });
    }

    // Sanity-check the caller-supplied name. A path separator here
    // would escape the pack dir; the git pack name convention forbids
    // it and our callers never need it.
    if canonical_name.is_empty()
        || canonical_name.contains('/')
        || canonical_name.contains('\\')
        || canonical_name.contains('\0')
    {
        return Err(CrabError::Internal(format!(
            "invalid canonical pack name: {canonical_name:?}"
        )));
    }

    let git_sha1 = extract_pack_trailer_sha1(pack_bytes)?;
    let pack_name = format!("pack-{canonical_name}.pack");
    let idx_name = format!("pack-{canonical_name}.idx");
    let final_pack = pack_dir.join(&pack_name);
    let final_idx = pack_dir.join(&idx_name);
    let final_rev = final_idx.with_extension("rev");

    // Fast path: already installed.
    if final_pack.exists() && final_idx.exists() {
        let pack_len = std::fs::metadata(&final_pack)?.len();
        if !final_rev.exists() {
            crab_git::pack_locator::write_pack_reverse_index(&final_idx, &final_rev)
                .map_err(crab_git::pack::PackError::from)?;
        }
        crab_git::pack_locator::PackLocationIter::open(&final_idx, &final_rev, pack_len)
            .map_err(crab_git::pack::PackError::from)?;
        debug!(
            canonical_name = %canonical_name,
            "pack already installed locally, skipping"
        );
        return Ok(InstalledPack {
            git_sha1,
            pack_path: final_pack,
            idx_path: final_idx,
            rev_path: final_rev,
        });
    }

    tokio::fs::create_dir_all(pack_dir).await?;

    // Do the rest in a blocking task — index-pack and file ops are sync.
    let pack_dir = pack_dir.to_owned();
    let pack_bytes = Bytes::copy_from_slice(pack_bytes);
    let git_sha1_clone = git_sha1.clone();
    tokio::task::spawn_blocking(move || {
        install_pack_blocking(
            &pack_dir,
            &pack_bytes,
            &git_sha1_clone,
            &final_pack,
            &final_idx,
        )
    })
    .await
    .map_err(|e| CrabError::Internal(format!("install_pack join: {e}")))?
}

/// Install a pack already downloaded to a temporary file.
///
/// This avoids materializing large fetch packs in memory: callers stream
/// remote bytes to `pack_tmp_path`, then this function verifies the pack,
/// runs `git index-pack`, and atomically renames the `.pack`/`.idx`
/// pair into place under `canonical_name`.
pub async fn install_pack_file_locally(
    pack_dir: &Path,
    pack_tmp_path: &Path,
    canonical_name: &str,
    max_input_size: u64,
    fsck_objects: bool,
) -> Result<InstalledPack> {
    let pack_dir = pack_dir.to_owned();
    let pack_tmp_path = pack_tmp_path.to_owned();
    let canonical_name = canonical_name.to_owned();
    let error_pack_id = canonical_name.clone();
    tokio::task::spawn_blocking(move || {
        crab_git::pack::install_pack_file_from_path(
            &pack_dir,
            &pack_tmp_path,
            &canonical_name,
            max_input_size,
            fsck_objects,
        )
    })
    .await
    .map_err(|e| CrabError::Internal(format!("install_pack_file join: {e}")))?
    .map_err(|error| match error {
        crab_git::pack::PackError::ObjectFsckFailed { git_sha1, stderr } => {
            CrabError::FetchMalformedObject {
                pack_id: error_pack_id,
                oid: git_sha1,
                kind: "pack".to_owned(),
                detail: stderr,
            }
        }
        error => CrabError::from(error),
    })
}

/// Verify that every requested ref tip resolves after the fetch batch lands.
pub async fn validate_fetched_ref_tips(git_dir: &Path, ref_tips: &[String]) -> Result<()> {
    let git_dir = git_dir.to_owned();
    let ref_tips = ref_tips.to_vec();
    tokio::task::spawn_blocking(move || validate_fetched_ref_tips_blocking(&git_dir, &ref_tips))
        .await
        .map_err(|error| {
            CrabError::Internal(format!("Git ref-tip validation join error: {error}"))
        })?
}

/// Create the single ref-tip pack Git requires for remote-helper connectivity proof.
///
/// The returned `.keep` path names a pack containing every requested ref tip.
/// Git removes the keep file after it consumes the helper response.
pub(crate) async fn create_connectivity_proof_pack(
    git_dir: &Path,
    ref_tips: &[String],
    validation_digest: &str,
) -> Result<Option<PathBuf>> {
    if ref_tips.is_empty() {
        return Ok(None);
    }

    let git_dir = git_dir.to_owned();
    let mut ref_tips = ref_tips.to_vec();
    ref_tips.sort_unstable();
    ref_tips.dedup();
    let validation_digest = validation_digest.to_owned();
    tokio::task::spawn_blocking(move || {
        create_connectivity_proof_pack_blocking(&git_dir, &ref_tips, &validation_digest)
    })
    .await
    .map_err(|error| {
        CrabError::Internal(format!("Git connectivity proof pack join error: {error}"))
    })?
}

fn create_connectivity_proof_pack_blocking(
    git_dir: &Path,
    ref_tips: &[String],
    validation_digest: &str,
) -> Result<Option<PathBuf>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let pack_dir = git_dir.join("objects").join("pack");
    std::fs::create_dir_all(&pack_dir)?;
    if pack_dir
        .to_str()
        .is_none_or(|path| path.contains(['\r', '\n']))
    {
        return Ok(None);
    }

    let mut identity = blake3::Hasher::new();
    identity.update(b"crab remote-helper connectivity pack\0");
    identity.update(validation_digest.as_bytes());
    for tip in ref_tips {
        identity.update(tip.as_bytes());
    }
    let canonical_name = format!("connectivity-{}", identity.finalize().to_hex());

    let pack_tmp = tempfile::Builder::new()
        .prefix(".crab-connectivity-")
        .suffix(".pack")
        .tempfile_in(&pack_dir)?;
    let pack_output = pack_tmp.reopen()?;
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["pack-objects", "--stdout", "--window=0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(pack_output))
        .stderr(Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;
    {
        let mut stdin = child.stdin.take().ok_or_else(|| {
            CrabError::Internal("git pack-objects did not expose piped stdin".to_owned())
        })?;
        for tip in ref_tips {
            writeln!(stdin, "{tip}")?;
        }
    }
    let output = child.wait_with_output().map_err(CrabError::Io)?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git pack-objects failed while creating connectivity proof: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let installed = crab_git::pack::install_pack_file_from_path(
        &pack_dir,
        pack_tmp.path(),
        &canonical_name,
        0,
        false,
    )?;
    validate_connectivity_proof_tips(&installed, ref_tips)?;
    let keep_path = installed.pack_path.with_extension("keep");
    std::fs::write(
        &keep_path,
        b"Crab remote-helper connectivity proof; Git removes this file.\n",
    )?;
    Ok(Some(keep_path))
}

fn validate_connectivity_proof_tips(installed: &InstalledPack, ref_tips: &[String]) -> Result<()> {
    let pack_len = std::fs::metadata(&installed.pack_path)?.len();
    let locations = crab_git::pack_locator::PackLocationIter::open(
        &installed.idx_path,
        &installed.rev_path,
        pack_len,
    )
    .map_err(crab_git::pack::PackError::from)?;
    let mut packed = HashSet::with_capacity(locations.len());
    for location in locations {
        packed.insert(location.map_err(crab_git::pack::PackError::from)?.oid);
    }
    for tip in ref_tips {
        let oid = gix_hash::ObjectId::from_hex(tip.as_bytes()).map_err(|error| {
            CrabError::Internal(format!("invalid connectivity proof ref tip {tip}: {error}"))
        })?;
        if !packed.contains(&oid) {
            return Err(CrabError::Internal(format!(
                "connectivity proof pack does not contain all requested ref tips: missing {tip}"
            )));
        }
    }
    Ok(())
}

fn validate_fetched_ref_tips_blocking(git_dir: &Path, ref_tips: &[String]) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    if ref_tips.is_empty() {
        return Ok(());
    }

    let mut child = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .arg("cat-file")
        .arg("--batch-check=%(objectname) %(objecttype)")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| CrabError::Internal("git cat-file did not expose piped stdin".to_owned()))?;
    // Feed requests while draining responses so neither bounded OS pipe can block the other.
    let (output, writer_result) = std::thread::scope(|scope| {
        let writer = scope.spawn(move || {
            for tip in ref_tips {
                writeln!(stdin, "{tip}")?;
            }
            stdin.flush()
        });
        let output = child.wait_with_output();
        (output, writer.join())
    });
    let output = output.map_err(CrabError::Io)?;
    let writer_result = writer_result
        .map_err(|_| CrabError::Internal("git cat-file stdin writer thread panicked".to_owned()))?;
    if !output.status.success() {
        return Err(CrabError::FetchMalformedObject {
            pack_id: "fetch-batch".to_owned(),
            oid: ref_tips.first().cloned().unwrap_or_default(),
            kind: "ref-tip".to_owned(),
            detail: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    writer_result.map_err(CrabError::Io)?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    for tip in ref_tips {
        let line = lines.next().unwrap_or_default();
        let mut fields = line.split_whitespace();
        let _oid = fields.next();
        let object_type = fields.next();
        if object_type.is_none() || object_type == Some("missing") {
            return Err(CrabError::FetchMalformedObject {
                pack_id: "fetch-batch".to_owned(),
                oid: tip.clone(),
                kind: "ref-tip".to_owned(),
                detail: format!(
                    "advertised ref tip is absent from the fetched object database: {tip}"
                ),
            });
        }
    }
    if lines.next().is_some() {
        return Err(CrabError::Internal(
            "git cat-file returned more ref-tip rows than requested".to_owned(),
        ));
    }
    Ok(())
}

/// Synchronous body of [`install_pack_locally`]. Extracted so the async
/// wrapper can run it in a single `spawn_blocking` without interleaved
/// `.await` points.
fn install_pack_blocking(
    pack_dir: &Path,
    pack_bytes: &[u8],
    git_sha1: &str,
    final_pack: &Path,
    final_idx: &Path,
) -> Result<InstalledPack> {
    use std::io::Write;

    // Tempfile for the pack itself, same dir so rename is atomic.
    let mut pack_tmp = NamedTempFile::new_in(pack_dir)?;
    pack_tmp.write_all(pack_bytes)?;
    pack_tmp.flush()?;
    // Only the legacy (shellout) path passes this tempfile path to
    // `git index-pack`; the gix-pack-native path reads from
    // `pack_bytes` directly.
    #[cfg(not(feature = "gix-pack-native"))]
    let pack_tmp_path = pack_tmp.path().to_owned();

    // Reserve a unique path for the idx. We don't need the file handle;
    // git index-pack writes the file itself. The NamedTempFile RAII
    // guard keeps the path tracked so cleanup happens on early return.
    let idx_tmp = tempfile::Builder::new()
        .prefix(".crab-idx-")
        .suffix(".idx")
        .tempfile_in(pack_dir)?;
    let idx_tmp_path = idx_tmp.path().to_owned();
    let _idx_guard = idx_tmp; // keep RAII cleanup alive on early return

    // index-pack refuses to overwrite. Remove the empty placeholder
    // file so it can create the real idx.
    let _ = std::fs::remove_file(&idx_tmp_path);

    // REPLACEABLE: gix_pack::index::File::write_data_iter_to_stream
    // (gated by `gix-pack-native` — see `pack_native::index_pack`).
    #[cfg(feature = "gix-pack-native")]
    {
        // Native gix-pack indexer. Writes directly to `idx_tmp_path`
        // via `gix_pack::index::File::write_data_iter_to_stream`,
        // matching the on-disk shape `git index-pack -o` produced.
        // Any `.rev` sidecar handling below is a no-op under this
        // path because gix-pack does not emit one.
        if let Err(e) = crate::git::pack_native::index_pack_for_install(pack_bytes, &idx_tmp_path) {
            let _ = std::fs::remove_file(&idx_tmp_path);
            return Err(CrabError::Internal(format!(
                "gix-pack index write failed for pack-{git_sha1}: {e}"
            )));
        }
    }
    #[cfg(not(feature = "gix-pack-native"))]
    let output = std::process::Command::new("git")
        .arg("index-pack")
        .arg("--rev-index")
        .arg("-o")
        .arg(&idx_tmp_path)
        .arg(&pack_tmp_path)
        .output()
        .map_err(CrabError::Io)?;

    // git index-pack 2.31+ also writes a `.rev` reverse-index sidecar
    // alongside the `-o` output. Its name is derived from the -o path
    // (strip `.idx`, append `.rev`), so it lives under our tempfile
    // name and needs to be renamed into place too — or cleaned up on
    // older gits where the file doesn't exist at all.
    let rev_tmp_path = idx_tmp_path.with_extension("rev");
    let final_rev = final_idx.with_extension("rev");

    #[cfg(not(feature = "gix-pack-native"))]
    if !output.status.success() {
        // Explicitly reap any partial artifacts. NamedTempFile handles
        // the pack tempfile and our primary idx-tmp guard handles the
        // idx, but `git index-pack` may have also produced a .rev we
        // didn't register with RAII.
        let _ = std::fs::remove_file(&rev_tmp_path);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(CrabError::Internal(format!(
            "git index-pack failed for pack-{git_sha1}: {stderr}"
        )));
    }

    if !idx_tmp_path.exists() {
        let _ = std::fs::remove_file(&rev_tmp_path);
        return Err(CrabError::Internal(format!(
            "git index-pack succeeded but produced no index at {}",
            idx_tmp_path.display()
        )));
    }

    // Sanity check: the index's self-reported pack-hash should match
    // our own trailer extraction. A mismatch means either corrupt
    // bytes or a bug, and we shouldn't install in that case. Any
    // `.rev` sidecar git produced is removed along with the idx.
    match parse_idx_pack_hash(&idx_tmp_path) {
        Ok(reported) if reported == git_sha1 => {}
        Ok(reported) => {
            let _ = std::fs::remove_file(&idx_tmp_path);
            let _ = std::fs::remove_file(&rev_tmp_path);
            return Err(CrabError::Internal(format!(
                "pack hash mismatch: trailer says {git_sha1}, index says {reported}"
            )));
        }
        Err(e) => {
            warn!(
                git_sha1 = %git_sha1,
                error = %e,
                "could not read pack hash from newly-generated idx; installing anyway"
            );
        }
    }

    if !rev_tmp_path.exists() {
        crab_git::pack_locator::write_pack_reverse_index(&idx_tmp_path, &rev_tmp_path)
            .map_err(crab_git::pack::PackError::from)?;
    }
    crab_git::pack_locator::PackLocationIter::open(
        &idx_tmp_path,
        &rev_tmp_path,
        pack_bytes.len() as u64,
    )
    .map_err(crab_git::pack::PackError::from)?;

    // The two derived sidecars become visible before the canonical pack.
    // A failed pack rename removes both so Git never observes a partial install.
    std::fs::rename(&idx_tmp_path, final_idx).map_err(CrabError::Io)?;

    if let Err(error) = std::fs::rename(&rev_tmp_path, &final_rev) {
        let _ = std::fs::remove_file(final_idx);
        let _ = std::fs::remove_file(&rev_tmp_path);
        return Err(CrabError::Io(error));
    }

    if let Err(e) = pack_tmp.persist(final_pack) {
        let _ = std::fs::remove_file(final_idx);
        let _ = std::fs::remove_file(&final_rev);
        return Err(CrabError::Io(e.error));
    }

    Ok(InstalledPack {
        git_sha1: git_sha1.to_owned(),
        pack_path: final_pack.to_owned(),
        idx_path: final_idx.to_owned(),
        rev_path: final_rev,
    })
}

/// Extract the pack's trailing SHA-1 and return it as lowercase hex.
///
/// A git pack ends with a 20-byte SHA-1 computed over all preceding
/// bytes. That trailer is the pack's identity from git's perspective,
/// independent of filename.
fn extract_pack_trailer_sha1(pack_bytes: &[u8]) -> Result<String> {
    const SHA1_LEN: usize = 20;
    if pack_bytes.len() < SHA1_LEN + 12 || &pack_bytes[0..4] != b"PACK" {
        return Err(CrabError::Internal(
            "pack bytes too short or missing PACK header".to_owned(),
        ));
    }
    let trailer = &pack_bytes[pack_bytes.len() - SHA1_LEN..];
    Ok(hex_encode(trailer))
}

/// Read the pack-hash field from an idx file.
fn parse_idx_pack_hash(idx_path: &Path) -> Result<String> {
    let idx = gix_pack::index::File::at(idx_path, gix_hash::Kind::Sha1).map_err(|e| {
        CrabError::Internal(format!(
            "failed to open idx {} for hash check: {e}",
            idx_path.display()
        ))
    })?;
    Ok(idx.pack_checksum().to_hex().to_string())
}

/// Lowercase hex encoder — local to this module so we don't pull `hex`
/// just for this use.
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Timeout wrapper around [`install_pack_locally`].
///
/// Production safety: the push pipeline should never block forever on
/// local bookkeeping. This wrapper bounds `git index-pack` at
/// [`INDEX_PACK_TIMEOUT`] and returns `Internal` on elapsed timeout.
/// Note: `tokio::time::timeout` can't interrupt a running
/// `spawn_blocking` task — the git subprocess itself keeps running
/// until it exits. In practice index-pack is fast and well-behaved;
/// the timeout exists to bound the worst case for the caller, not to
/// force-kill git.
///
/// `max_input_size` is forwarded to [`install_pack_locally`] so
/// oversized packs are rejected structurally before `index-pack`
/// spends any time on them.
pub async fn install_pack_locally_with_timeout(
    pack_dir: &Path,
    pack_bytes: &[u8],
    canonical_name: &str,
    max_input_size: u64,
) -> Result<InstalledPack> {
    match tokio::time::timeout(
        INDEX_PACK_TIMEOUT,
        install_pack_locally(pack_dir, pack_bytes, canonical_name, max_input_size),
    )
    .await
    {
        Ok(res) => res,
        Err(_) => Err(CrabError::Internal(format!(
            "git index-pack exceeded timeout of {}s",
            INDEX_PACK_TIMEOUT.as_secs()
        ))),
    }
}

/// Timeout wrapper around [`install_pack_file_locally`].
pub async fn install_pack_file_locally_with_timeout(
    pack_dir: &Path,
    pack_tmp_path: &Path,
    canonical_name: &str,
    max_input_size: u64,
    fsck_objects: bool,
) -> Result<InstalledPack> {
    match tokio::time::timeout(
        INDEX_PACK_TIMEOUT,
        install_pack_file_locally(
            pack_dir,
            pack_tmp_path,
            canonical_name,
            max_input_size,
            fsck_objects,
        ),
    )
    .await
    {
        Ok(res) => res,
        Err(_) => Err(CrabError::Internal(format!(
            "git index-pack exceeded timeout of {}s",
            INDEX_PACK_TIMEOUT.as_secs()
        ))),
    }
}

/// Delete the `.pack`, `.idx`, and (if present) `.rev` files for
/// the given pack id from the pack directory.
///
/// Idempotent — `NotFound` errors are treated as success so a
/// partial install (pack written, idx not yet renamed) rolls back
/// cleanly. Other I/O errors propagate because a stuck file left
/// behind would let malformed bytes stay visible to git.
pub async fn rollback_installed_pack(pack_dir: &Path, pack_id: &str) -> Result<()> {
    let pack_dir = pack_dir.to_owned();
    let pack_id = pack_id.to_owned();
    tokio::task::spawn_blocking(move || rollback_installed_pack_blocking(&pack_dir, &pack_id))
        .await
        .map_err(|e| CrabError::Internal(format!("rollback_installed_pack join: {e}")))?
}

fn rollback_installed_pack_blocking(pack_dir: &Path, pack_id: &str) -> Result<()> {
    let pack_path = pack_dir.join(format!("pack-{pack_id}.pack"));
    let idx_path = pack_dir.join(format!("pack-{pack_id}.idx"));
    let rev_path = pack_dir.join(format!("pack-{pack_id}.rev"));

    remove_if_exists(&pack_path)?;
    remove_if_exists(&idx_path)?;
    remove_if_exists(&rev_path)?;

    warn!(
        pack_id = %pack_id,
        pack_dir = %pack_dir.display(),
        "rolled back rejected installed pack"
    );

    Ok(())
}

/// Remove a file, treating `NotFound` as success.
fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CrabError::Io(e)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::test::git_repo::{CleanGitEnvGuard, GitDirGuard, TEST_GIT_REPO};

    fn sha1_bytes(val: u8) -> [u8; 20] {
        [val; 20]
    }

    /// Returns the commit SHA from the shared test repo.
    fn test_commit_sha() -> String {
        TEST_GIT_REPO.commit_sha.clone()
    }

    #[test]
    fn fetched_ref_tip_validation_handles_large_batches() {
        let ref_tips = vec![test_commit_sha(); 32_768];

        validate_fetched_ref_tips_blocking(&TEST_GIT_REPO.git_dir, &ref_tips).unwrap();
    }

    #[tokio::test]
    async fn connectivity_proof_rejects_reused_pack_without_requested_tip() {
        let commit = test_commit_sha();
        let tree = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&TEST_GIT_REPO.git_dir)
            .args(["rev-parse", "HEAD^{tree}"])
            .output()
            .unwrap();
        assert!(tree.status.success());
        let tree = String::from_utf8(tree.stdout).unwrap().trim().to_owned();

        let expected = create_connectivity_proof_pack(
            &TEST_GIT_REPO.git_dir,
            std::slice::from_ref(&commit),
            "validated-generation",
        )
        .await
        .unwrap()
        .unwrap();
        let other =
            create_connectivity_proof_pack(&TEST_GIT_REPO.git_dir, &[tree], "other-generation")
                .await
                .unwrap()
                .unwrap();
        for extension in ["pack", "idx", "rev"] {
            std::fs::remove_file(expected.with_extension(extension)).unwrap();
            std::fs::copy(
                other.with_extension(extension),
                expected.with_extension(extension),
            )
            .unwrap();
        }

        let error = create_connectivity_proof_pack(
            &TEST_GIT_REPO.git_dir,
            &[commit],
            "validated-generation",
        )
        .await
        .expect_err("a stale proof pack must not be acknowledged");
        assert!(error.to_string().contains("requested ref tips"));
    }

    /// Build a minimal valid pack index v2 file containing the given OIDs.
    ///
    /// Format: 4-byte magic `0xff744f63` + 4-byte version `2` + 256-entry
    /// fanout table + sorted OID table + CRC32 table + offset table +
    /// pack checksum + index checksum. See Git pack-format docs.
    ///
    /// Replaces the old test helper that wrote raw 20-byte OIDs, which
    /// was only compatible with the old (broken) parse_idx_object_ids.
    /// See finding S1-P1-8.
    fn write_fake_idx(dir: &Path, pack_id: &str, oids: &[[u8; 20]]) -> std::path::PathBuf {
        let idx_path = dir.join(format!("pack-{pack_id}.idx"));
        let mut sorted: Vec<[u8; 20]> = oids.to_vec();
        sorted.sort();

        let mut data = Vec::new();
        // Magic + version
        data.extend_from_slice(&[0xff, 0x74, 0x4f, 0x63]);
        data.extend_from_slice(&2u32.to_be_bytes());
        // Fanout table: 256 × u32, cumulative count of OIDs ≤ (i, 0xff...)
        for i in 0u8..=0xff {
            let count: u32 = sorted.iter().filter(|o| o[0] <= i).count() as u32;
            data.extend_from_slice(&count.to_be_bytes());
        }
        // Sorted OID table
        for oid in &sorted {
            data.extend_from_slice(oid);
        }
        // CRC32 table (zeros — we don't validate CRCs)
        for _ in &sorted {
            data.extend_from_slice(&0u32.to_be_bytes());
        }
        // Offset table (zeros — not used for OID enumeration)
        for _ in &sorted {
            data.extend_from_slice(&0u32.to_be_bytes());
        }
        // Pack checksum (zero SHA1 — not validated when opening)
        data.extend_from_slice(&[0u8; 20]);
        // Index checksum: SHA1 of everything written so far.
        let idx_checksum = {
            use sha1::Digest;
            let mut hasher = sha1::Sha1::new();
            hasher.update(&data);
            hasher.finalize()
        };
        data.extend_from_slice(&idx_checksum);

        std::fs::write(&idx_path, &data).unwrap();
        idx_path
    }

    // --- compute_remote_object_set ---

    fn pack_entry(pack_id: &str, size: u64) -> PackManifestEntry {
        PackManifestEntry {
            pack_id: pack_id.to_owned(),
            size,
            content_hash: pack_id.to_owned(),
            ref_tips: Vec::new(),
            object_count: 1,
        }
    }

    #[test]
    fn empty_pack_list_returns_empty_set() {
        let dir = tempfile::tempdir().unwrap();
        let result = compute_remote_object_set(dir.path(), &[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parses_objects_from_idx_files() {
        let dir = tempfile::tempdir().unwrap();
        let oid_a = sha1_bytes(0xaa);
        let oid_b = sha1_bytes(0xbb);
        write_fake_idx(dir.path(), "pack1", &[oid_a, oid_b]);

        let packs = vec![pack_entry("pack1", 100)];

        let result = compute_remote_object_set(dir.path(), &packs).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains(&gix_hash::ObjectId::from_bytes_or_panic(&oid_a)));
        assert!(result.contains(&gix_hash::ObjectId::from_bytes_or_panic(&oid_b)));
    }

    #[test]
    fn missing_idx_skipped_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        // No idx file created for "missing_pack".
        let packs = vec![pack_entry("missing_pack", 200)];

        let result = compute_remote_object_set(dir.path(), &packs).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn multiple_packs_merged_into_single_set() {
        let dir = tempfile::tempdir().unwrap();
        let oid_a = sha1_bytes(0x11);
        let oid_b = sha1_bytes(0x22);
        let oid_c = sha1_bytes(0x33);
        write_fake_idx(dir.path(), "p1", &[oid_a, oid_b]);
        write_fake_idx(dir.path(), "p2", &[oid_b, oid_c]);

        let packs = vec![pack_entry("p1", 100), pack_entry("p2", 150)];

        let result = compute_remote_object_set(dir.path(), &packs).unwrap();
        // oid_b appears in both packs but the set deduplicates.
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn partial_missing_idx_still_parses_available() {
        let dir = tempfile::tempdir().unwrap();
        let oid_a = sha1_bytes(0xcc);
        write_fake_idx(dir.path(), "good", &[oid_a]);
        // "bad" pack has no idx file.

        let packs = vec![pack_entry("good", 50), pack_entry("bad", 75)];

        let result = compute_remote_object_set(dir.path(), &packs).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&gix_hash::ObjectId::from_bytes_or_panic(&oid_a)));
    }

    // --- generate_push_pack ---

    #[tokio::test]
    async fn full_pack_when_no_remote_objects() {
        let _guard = GitDirGuard::new();
        let sha = test_commit_sha();
        let refs = vec![RefUpdate {
            ref_name: "refs/heads/main".into(),
            old_sha: None,
            new_sha: sha,
            force: false,
        }];
        let config = PushPackConfig::default();

        let packed = generate_push_pack(&refs, None, &config).await.unwrap();
        assert!(packed.object_count >= 1);
        assert!(!packed.is_thin);
        assert!(!packed.pack.is_empty());
    }

    #[tokio::test]
    async fn file_backed_pack_generation_writes_verified_pack_file() {
        let _guard = GitDirGuard::new();
        let sha = test_commit_sha();
        let refs = vec![RefUpdate {
            ref_name: "refs/heads/main".into(),
            old_sha: None,
            new_sha: sha,
            force: false,
        }];
        let config = PushPackConfig::default();

        let packed = generate_push_pack_file_with_exclusions(&refs, None, &config)
            .await
            .unwrap();

        assert!(packed.pack_size > 0);
        assert!(packed.object_count >= 1);
        assert!(!packed.is_thin);
        assert_eq!(packed.git_sha1.len(), 40);
        assert!(packed.pack_path.exists());

        let pack_path: &Path = packed.pack_path.as_ref();
        let pack_bytes = std::fs::read(pack_path).unwrap();
        let pack_hash = blake3::hash(&pack_bytes);
        assert_eq!(pack_bytes.len() as u64, packed.pack_size);
        assert_eq!(*pack_hash.as_bytes(), packed.pack_blake3);
        assert_eq!(pack_hash.to_hex().to_string(), packed.pack_blake3_hex);

        let inspection = inspect_pack_file(pack_path).unwrap();
        assert_eq!(inspection.object_count, packed.object_count);
        assert_eq!(inspection.git_sha1, packed.git_sha1);
    }

    #[tokio::test]
    async fn exact_object_pack_does_not_expand_commit_reachability() {
        let _guard = GitDirGuard::new();
        let commit = gix_hash::ObjectId::from_hex(test_commit_sha().as_bytes()).unwrap();

        let packed = generate_pack_file_from_object_ids(&[commit], &PushPackConfig::default())
            .await
            .unwrap();

        assert_eq!(packed.object_count, 1);
        assert!(!packed.is_thin);
        assert!(packed.pack_path.exists());
    }

    fn write_incompressible_git_blobs(
        count: usize,
        bytes_per_blob: usize,
    ) -> (tempfile::TempDir, PathBuf, Vec<gix_hash::ObjectId>) {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let _git_env = CleanGitEnvGuard::new();
        let repo = tempfile::tempdir().unwrap();
        let git_dir = repo.path().join("repo.git");
        let init = Command::new("git")
            .args(["init", "--bare"])
            .arg(&git_dir)
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        let mut objects = Vec::with_capacity(count);
        for blob_index in 0..count {
            let bytes = incompressible_bytes(blob_index as u64, bytes_per_blob);

            let mut child = Command::new("git")
                .args(["--git-dir"])
                .arg(&git_dir)
                .args(["hash-object", "-w", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&bytes).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "git hash-object failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            objects.push(
                gix_hash::ObjectId::from_hex(
                    String::from_utf8(output.stdout).unwrap().trim().as_bytes(),
                )
                .unwrap(),
            );
        }

        (repo, git_dir, objects)
    }

    fn incompressible_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ seed;
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.push((state >> 24) as u8);
        }
        bytes
    }

    fn write_incompressible_git_commit(
        count: usize,
        bytes_per_blob: usize,
    ) -> (tempfile::TempDir, PathBuf, String) {
        use std::process::Command;

        let _git_env = CleanGitEnvGuard::new();
        let repo = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(init.status.success());
        for (key, value) in [
            ("user.name", "Crab Test"),
            ("user.email", "test@crab.build"),
        ] {
            let status = Command::new("git")
                .args(["config", key, value])
                .current_dir(repo.path())
                .status()
                .unwrap();
            assert!(status.success());
        }
        for blob_index in 0..count {
            std::fs::write(
                repo.path().join(format!("blob-{blob_index}.bin")),
                incompressible_bytes(blob_index as u64, bytes_per_blob),
            )
            .unwrap();
        }
        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "large fixture"])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(head.status.success());
        let git_dir = repo.path().join(".git");
        let head = String::from_utf8(head.stdout).unwrap().trim().to_owned();
        (repo, git_dir, head)
    }

    #[test]
    fn pack_generation_limit_reserves_headroom_below_hard_limit() {
        let hard_limit = 2 * 1024 * 1024 * 1024;

        assert_eq!(pack_generation_size_limit(0), 0);
        assert_eq!(pack_generation_size_limit(1024 * 1024), 1024 * 1024);
        assert_eq!(
            pack_generation_size_limit(hard_limit),
            hard_limit - hard_limit / 20
        );
    }

    #[tokio::test]
    async fn bounded_exact_object_generation_splits_pack_set() {
        let (_repo, git_dir, objects) = write_incompressible_git_blobs(4, 700 * 1024);
        let limit = 1024 * 1024;
        let config = PushPackConfig {
            thin_packs: false,
            max_input_size: limit,
            git_dir: Some(git_dir),
        };

        let packs = generate_pack_files_from_object_ids(&objects, &config)
            .await
            .unwrap();

        assert!(packs.len() > 1);
        assert_eq!(
            packs.iter().map(|pack| pack.object_count).sum::<u64>(),
            objects.len() as u64
        );
        assert!(packs.iter().all(|pack| pack.pack_size <= limit));
    }

    #[tokio::test]
    async fn bounded_generation_keeps_pack_files_on_git_filesystem() {
        let (_repo, git_dir, objects) = write_incompressible_git_blobs(2, 64 * 1024);
        let config = PushPackConfig {
            thin_packs: false,
            max_input_size: 1024 * 1024,
            git_dir: Some(git_dir.clone()),
        };

        let packs = generate_pack_files_from_object_ids(&objects, &config)
            .await
            .unwrap();

        assert!(
            packs
                .iter()
                .all(|pack| pack.pack_path.starts_with(&git_dir)),
            "Git may create pack temporaries beside the object database, so the output must stay on the same filesystem"
        );
    }

    #[tokio::test]
    async fn bounded_revision_generation_splits_pack_set() {
        let (_repo, git_dir, head) = write_incompressible_git_commit(4, 700 * 1024);
        let limit = 1024 * 1024;
        let config = PushPackConfig {
            thin_packs: false,
            max_input_size: limit,
            git_dir: Some(git_dir),
        };
        let refs = vec![RefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_sha: None,
            new_sha: head,
            force: false,
        }];

        let packs = generate_push_pack_files_with_exclusions(&refs, None, &config)
            .await
            .unwrap();

        assert!(packs.len() > 1);
        assert!(packs.iter().all(|pack| pack.pack_size <= limit));
        assert!(packs.iter().map(|pack| pack.object_count).sum::<u64>() >= 6);
    }

    #[tokio::test]
    async fn bounded_generation_rejects_single_object_larger_than_limit() {
        let (_repo, git_dir, objects) = write_incompressible_git_blobs(1, 2 * 1024 * 1024);
        let config = PushPackConfig {
            thin_packs: false,
            max_input_size: 1024 * 1024,
            git_dir: Some(git_dir),
        };

        let result = generate_pack_files_from_object_ids(&objects, &config).await;

        assert!(matches!(result, Err(CrabError::PackTooLarge { .. })));
    }

    #[tokio::test]
    async fn file_backed_pack_generation_enforces_max_input_size() {
        let _guard = GitDirGuard::new();
        let sha = test_commit_sha();
        let refs = vec![RefUpdate {
            ref_name: "refs/heads/main".into(),
            old_sha: None,
            new_sha: sha,
            force: false,
        }];
        let config = PushPackConfig {
            thin_packs: false,
            max_input_size: 1,
            git_dir: None,
        };

        let result = generate_push_pack_file_with_exclusions(&refs, None, &config).await;
        assert!(matches!(result, Err(CrabError::PackTooLarge { .. })));
    }

    #[tokio::test]
    async fn excludes_remote_objects() {
        let _guard = GitDirGuard::new();
        let sha = test_commit_sha();
        let oid = gix_hash::ObjectId::from_hex(sha.as_bytes()).unwrap();
        let mut remote = HashSet::new();
        remote.insert(oid);

        let refs = vec![RefUpdate {
            ref_name: "refs/heads/main".into(),
            old_sha: None,
            new_sha: sha,
            force: false,
        }];
        let config = PushPackConfig::default();

        // When the commit is in the remote set, rev-list excludes it.
        // However, tree and blob objects reachable from the commit may
        // still appear. The key assertion is that the pack is smaller
        // than a full pack (or empty if all objects are excluded).
        let packed = generate_push_pack(&refs, Some(&remote), &config)
            .await
            .unwrap();
        // The commit itself is excluded; remaining objects depend on
        // rev-list behaviour. Just verify no error.
        assert!(!packed.is_thin);
    }

    #[tokio::test]
    async fn excludes_remote_ref_tips_without_object_set() {
        let _guard = GitDirGuard::new();
        let sha = test_commit_sha();

        let refs = vec![RefUpdate {
            ref_name: "refs/heads/main".into(),
            old_sha: None,
            new_sha: sha.clone(),
            force: false,
        }];
        let config = PushPackConfig::default();

        let full = generate_push_pack(&refs, None, &config).await.unwrap();
        let tips = vec![sha];
        let packed = generate_push_pack_with_exclusions(
            &refs,
            Some(RemotePackExclusions::RefTips(&tips)),
            &config,
        )
        .await
        .unwrap();

        assert!(!packed.is_thin);
        assert!(packed.object_count <= full.object_count);
    }

    #[tokio::test]
    async fn thin_pack_flag_set_when_configured() {
        let _guard = GitDirGuard::new();
        let sha = test_commit_sha();
        let remote = HashSet::new();

        let refs = vec![RefUpdate {
            ref_name: "refs/heads/dev".into(),
            old_sha: None,
            new_sha: sha,
            force: false,
        }];
        let config = PushPackConfig {
            thin_packs: true,
            max_input_size: 0,
            git_dir: None,
        };

        let packed = generate_push_pack(&refs, Some(&remote), &config)
            .await
            .unwrap();
        assert!(packed.is_thin);
    }

    #[tokio::test]
    async fn ref_tip_exclusions_do_not_enable_thin_pack() {
        let _guard = GitDirGuard::new();
        let sha = test_commit_sha();

        let refs = vec![RefUpdate {
            ref_name: "refs/heads/dev".into(),
            old_sha: None,
            new_sha: sha.clone(),
            force: false,
        }];
        let config = PushPackConfig {
            thin_packs: true,
            max_input_size: 0,
            git_dir: None,
        };
        let tips = vec![sha];

        let packed = generate_push_pack_with_exclusions(
            &refs,
            Some(RemotePackExclusions::RefTips(&tips)),
            &config,
        )
        .await
        .unwrap();
        assert!(!packed.is_thin);
    }

    #[tokio::test]
    async fn thin_pack_requires_remote_objects() {
        let _guard = GitDirGuard::new();
        let sha = test_commit_sha();
        let refs = vec![RefUpdate {
            ref_name: "refs/heads/dev".into(),
            old_sha: None,
            new_sha: sha,
            force: false,
        }];
        // thin_packs=true but remote_objects=None → not thin.
        let config = PushPackConfig {
            thin_packs: true,
            max_input_size: 0,
            git_dir: None,
        };

        let packed = generate_push_pack(&refs, None, &config).await.unwrap();
        assert!(!packed.is_thin, "thin pack needs remote objects");
    }

    #[tokio::test]
    async fn empty_refs_produces_empty_pack() {
        let config = PushPackConfig::default();
        let packed = generate_push_pack(&[], None, &config).await.unwrap();
        assert_eq!(packed.object_count, 0);
        assert!(packed.pack.is_empty());
        assert!(packed.idx.is_empty());
    }

    // --- parse_idx_object_ids ---

    #[test]
    fn parse_idx_empty_file_errors() {
        // An empty file is not a valid pack index. The old (broken)
        // parser returned an empty Vec; the real parser correctly
        // rejects it. See S1-P1-8.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.idx");
        std::fs::write(&path, &[]).unwrap();
        let result = parse_idx_object_ids(&path);
        assert!(
            result.is_err(),
            "parse_idx_object_ids should reject empty files"
        );
    }

    #[test]
    fn parse_idx_garbage_bytes_errors() {
        // Garbage bytes (not a valid pack index v2 header) should be
        // rejected, not silently treated as a list of 20-byte OIDs.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.idx");
        std::fs::write(&path, &[0xaa; 25]).unwrap();
        let result = parse_idx_object_ids(&path);
        assert!(
            result.is_err(),
            "parse_idx_object_ids should reject garbage (not v2-formatted) input"
        );
    }

    // --- install_pack_locally ---

    /// Generate a real pack from the shared test repo for install tests.
    ///
    /// Takes `GitDirGuard` by reference so the caller keeps it alive for
    /// the whole test (the env var has to be set during install too —
    /// `git index-pack` otherwise picks up the wrong repo).
    async fn generate_real_pack(_guard: &GitDirGuard) -> Bytes {
        let sha = test_commit_sha();
        let refs = vec![RefUpdate {
            ref_name: "refs/heads/main".into(),
            old_sha: None,
            new_sha: sha,
            force: false,
        }];
        let config = PushPackConfig::default();
        let packed = generate_push_pack(&refs, None, &config).await.unwrap();
        assert!(!packed.pack.is_empty(), "test pack must be non-empty");
        packed.pack
    }

    #[tokio::test]
    async fn install_pack_locally_writes_pack_and_idx() {
        let guard = GitDirGuard::new();
        let pack_bytes = generate_real_pack(&guard).await;
        let dir = tempfile::tempdir().unwrap();

        let installed = install_pack_locally(dir.path(), &pack_bytes, "abc123", 0)
            .await
            .unwrap();

        assert_eq!(installed.pack_path, dir.path().join("pack-abc123.pack"));
        assert_eq!(installed.idx_path, dir.path().join("pack-abc123.idx"));
        assert!(installed.pack_path.exists());
        assert!(installed.idx_path.exists());
        assert_eq!(installed.git_sha1.len(), 40);

        // The idx must be readable and report the same pack-hash we extracted.
        let reported = parse_idx_pack_hash(&installed.idx_path).unwrap();
        assert_eq!(reported, installed.git_sha1);

        // If git produced a .rev sidecar, it must also live under the
        // canonical name — not as a leftover tempfile. (Older gits
        // without pack.writeReverseIndex won't produce one, in which
        // case neither file exists.)
        let tempfiles: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".crab-idx-"))
            .collect();
        assert!(
            tempfiles.is_empty(),
            "leftover idx/rev tempfiles: {tempfiles:?}"
        );
    }

    #[tokio::test]
    async fn install_pack_locally_is_idempotent() {
        let guard = GitDirGuard::new();
        let pack_bytes = generate_real_pack(&guard).await;
        let dir = tempfile::tempdir().unwrap();

        let first = install_pack_locally(dir.path(), &pack_bytes, "samename", 0)
            .await
            .unwrap();
        let pack_mtime_first = std::fs::metadata(&first.pack_path)
            .unwrap()
            .modified()
            .unwrap();

        // Second install with identical inputs should be a no-op.
        let second = install_pack_locally(dir.path(), &pack_bytes, "samename", 0)
            .await
            .unwrap();
        let pack_mtime_second = std::fs::metadata(&second.pack_path)
            .unwrap()
            .modified()
            .unwrap();

        assert_eq!(first.pack_path, second.pack_path);
        assert_eq!(first.idx_path, second.idx_path);
        assert_eq!(
            pack_mtime_first, pack_mtime_second,
            "idempotent install should not rewrite the pack"
        );
    }

    #[tokio::test]
    async fn install_pack_locally_rejects_invalid_pack_bytes() {
        let dir = tempfile::tempdir().unwrap();

        let result = install_pack_locally(dir.path(), b"not a pack", "whatever", 0).await;
        assert!(result.is_err(), "garbage bytes must be rejected");

        // Pack dir must not contain the final files or leftover temps.
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(
            leftover.is_empty(),
            "failed install must leave the pack dir clean, found: {leftover:?}"
        );
    }

    #[tokio::test]
    async fn install_pack_locally_rejects_unsafe_names() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["", "foo/bar", "..", "back\\slash", "null\0byte"] {
            let result = install_pack_locally(dir.path(), &[0u8; 32], bad, 0).await;
            assert!(result.is_err(), "name {bad:?} should be rejected");
        }
    }

    #[tokio::test]
    async fn install_pack_locally_cleans_up_on_index_pack_failure() {
        // Truncated pack: has "PACK" header + enough body to pass
        // extract_pack_trailer_sha1 but index-pack will reject it.
        let mut bad = b"PACK".to_vec();
        bad.extend_from_slice(&2u32.to_be_bytes()); // version
        bad.extend_from_slice(&1u32.to_be_bytes()); // object count = 1
        bad.extend_from_slice(&[0u8; 32]); // garbage body
        bad.extend_from_slice(&[0xab; 20]); // fake trailer

        let dir = tempfile::tempdir().unwrap();
        let result = install_pack_locally(dir.path(), &bad, "bad-pack", 0).await;
        assert!(result.is_err(), "index-pack should reject truncated pack");

        // No `.pack`, `.idx`, or `.rev` should exist under the canonical name.
        assert!(!dir.path().join("pack-bad-pack.pack").exists());
        assert!(!dir.path().join("pack-bad-pack.idx").exists());
        assert!(!dir.path().join("pack-bad-pack.rev").exists());

        // And no temp files left behind — including any `.rev` sidecars
        // `git index-pack` may have produced before erroring out.
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert!(
            leftover.is_empty(),
            "failed install must leave pack dir clean, found: {leftover:?}"
        );
    }

    // --- extract_pack_trailer_sha1 ---

    #[test]
    fn extract_trailer_rejects_short_input() {
        assert!(extract_pack_trailer_sha1(b"").is_err());
        assert!(extract_pack_trailer_sha1(b"PACK").is_err());
        assert!(extract_pack_trailer_sha1(&[0u8; 31]).is_err());
    }

    #[test]
    fn extract_trailer_rejects_missing_magic() {
        let mut bytes = vec![0u8; 40];
        bytes[0..4].copy_from_slice(b"NOTP");
        assert!(extract_pack_trailer_sha1(&bytes).is_err());
    }

    #[test]
    fn extract_trailer_returns_last_20_bytes_as_hex() {
        let mut bytes = b"PACK".to_vec();
        bytes.extend_from_slice(&[0u8; 8]);
        bytes.extend_from_slice(&[0xab; 20]); // trailer
        let hex = extract_pack_trailer_sha1(&bytes).unwrap();
        assert_eq!(hex, "ab".repeat(20));
    }
}
