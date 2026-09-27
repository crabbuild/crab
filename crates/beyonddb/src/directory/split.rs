//! Metadata leaf copy, publication and child-opening fences.

use super::*;

impl DirectorySplit {
    pub(super) fn valid(&self) -> bool {
        let [left, right] = &self.children;
        self.parent.valid()
            && self.version > 0
            && left.valid()
            && right.valid()
            && self.parent.depth.checked_add(1) == Some(left.depth)
            && left.depth == right.depth
            && left.table_id == self.parent.table_id
            && right.table_id == self.parent.table_id
            && left.lower == self.parent.lower
            && left.upper == Some(right.lower)
            && right.upper == self.parent.upper
            && left.node_id == child_id(&self.parent, self.version, 0)
            && right.node_id == child_id(&self.parent, self.version, 1)
            && left.node_id != right.node_id
            && left.node_id != self.parent.node_id
            && right.node_id != self.parent.node_id
    }
}

fn child_id(parent: &DirectorySpec, version: u64, side: u8) -> [u8; 16] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"beyonddb.directory-child.v1\0");
    hash.update(parent.table_id.as_bytes());
    hash.update(&parent.node_id);
    hash.update(&version.to_be_bytes());
    hash.update(&[side]);
    let mut id = [0; 16];
    id.copy_from_slice(&hash.finalize().as_bytes()[..16]);
    id
}

/// Freeze one exact leaf version and persist its immutable copy identities.
pub struct FreezeDirectory;
impl Command for FreezeDirectory {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 5;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<u64>;
    type Output = Json<Option<DirectorySplit>>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(version): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let Some(mut state) = state(|batch| context.sql(batch))? else {
            return Ok(CommandResult::Rejected(Json(None)));
        };
        match &state.mode {
            DirectoryMode::Frozen(split) | DirectoryMode::Branch(split)
                if split.version == version =>
            {
                return Ok(CommandResult::Success(Json(Some(split.clone()))));
            }
            DirectoryMode::Leaf if state.version == version => {}
            _ => return Ok(CommandResult::Rejected(Json(None))),
        }
        if !context.sql(&statement(
            "SELECT 1 FROM ddb_directory_changes LIMIT 1",
            vec![],
        ))?[0]
            .rows
            .is_empty()
        {
            return Ok(CommandResult::Rejected(Json(None)));
        }
        let rows = all_ranges(context)?;
        if rows.len() < 2 {
            return Ok(CommandResult::Rejected(Json(None)));
        }
        let middle = rows.len() / 2;
        let boundary = rows[middle].lower;
        let depth = state
            .spec
            .depth
            .checked_add(1)
            .ok_or(Error::Command("directory depth overflow"))?;
        let left = DirectorySpec {
            node_id: child_id(&state.spec, version, 0),
            upper: Some(boundary),
            depth,
            ..state.spec.clone()
        };
        let right = DirectorySpec {
            node_id: child_id(&state.spec, version, 1),
            lower: boundary,
            depth,
            ..state.spec.clone()
        };
        let split = DirectorySplit {
            parent: state.spec.clone(),
            version,
            fingerprints: [
                fingerprint(&left, &rows[..middle])?,
                fingerprint(&right, &rows[middle..])?,
            ],
            children: [left, right],
        };
        if !split.valid() {
            return Ok(CommandResult::Rejected(Json(None)));
        }
        state.mode = DirectoryMode::Frozen(split.clone());
        save(context, &state)?;
        Ok(CommandResult::Success(Json(Some(split))))
    }
}

/// A child-copy receipt supplied by the authenticated directory controller.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectoryCopyReceipt {
    pub cell_id: [u8; 32],
    pub sequence: u64,
    pub fingerprint: [u8; 32],
}

/// Immutable split with receipts from both verified child installations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectorySplitPublication {
    pub split: DirectorySplit,
    pub receipts: [DirectoryCopyReceipt; 2],
}

/// Publish child references in the parent after both child copies are durable.
pub struct PublishDirectorySplit;
impl Command for PublishDirectorySplit {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 6;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<DirectorySplitPublication>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let Some(mut state) = state(|batch| context.sql(batch))? else {
            return Ok(CommandResult::Rejected(Json(false)));
        };
        if !input.split.valid() {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        for (index, receipt) in input.receipts.iter().enumerate() {
            let target =
                target_for_tenant(context.target().tenant(), &input.split.children[index])?;
            if receipt.cell_id != *target.cell_id().as_bytes()
                || (receipt.sequence == 0 || receipt.sequence > i64::MAX as u64)
                || receipt.fingerprint != input.split.fingerprints[index]
            {
                return Ok(CommandResult::Rejected(Json(false)));
            }
        }
        match &state.mode {
            DirectoryMode::Branch(split) if split == &input.split => {
                return Ok(CommandResult::Success(Json(true)));
            }
            DirectoryMode::Frozen(split) if split == &input.split => {}
            _ => return Ok(CommandResult::Rejected(Json(false))),
        }
        state.version = state
            .version
            .checked_add(1)
            .ok_or(Error::Command("directory version overflow"))?;
        state.mode = DirectoryMode::Branch(input.split);
        // The immutable split is retained to finish child opening after restart.
        // At most 1,024 compact rows are removed; the parent never stores descendants.
        context.sql(&statement("DELETE FROM ddb_directory_ranges", vec![]))?;
        save(context, &state)?;
        Ok(CommandResult::Success(Json(true)))
    }
}

/// Open a copied child after its parent publishes the matching split.
pub struct OpenDirectory;
impl Command for OpenDirectory {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 7;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<DirectorySplit>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let Some(mut state) = state(|batch| context.sql(batch))? else {
            return Ok(CommandResult::Rejected(Json(false)));
        };
        let Some(position) = input.children.iter().position(|child| child == &state.spec) else {
            return Ok(CommandResult::Rejected(Json(false)));
        };
        if !input.valid() {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        if matches!(
            state.mode,
            DirectoryMode::Retiring { .. } | DirectoryMode::Retired
        ) || state.initial_fingerprint != input.fingerprints[position]
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        if !matches!(state.mode, DirectoryMode::Importing) {
            return Ok(CommandResult::Success(Json(true)));
        }
        state.mode = DirectoryMode::Leaf;
        save(context, &state)?;
        Ok(CommandResult::Success(Json(true)))
    }
}
