//! Fence published descendants before acknowledging a directory generation's retirement.

use super::*;

/// Fence one exact directory generation and expose its pending child retirements.
pub struct RetireDirectory;
impl Command for RetireDirectory {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 8;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<DirectorySpec>;
    type Output = Json<Option<DirectoryMode>>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(spec): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let Some(mut state) = state(|batch| context.sql(batch))? else {
            return Ok(CommandResult::Rejected(Json(None)));
        };
        if state.spec != spec {
            return Ok(CommandResult::Rejected(Json(None)));
        }
        let mode = match state.mode {
            DirectoryMode::Retiring { .. } | DirectoryMode::Retired => {
                return Ok(CommandResult::Success(Json(Some(state.mode))));
            }
            DirectoryMode::Branch(split) => DirectoryMode::Retiring {
                children: split.children,
                acknowledged: 0,
            },
            // An unpublished copy cannot be opened by a correct controller:
            // publication still requires this parent to be Frozen. Fencing the
            // parent therefore also invalidates every unfinished copy attempt.
            DirectoryMode::Leaf | DirectoryMode::Frozen(_) | DirectoryMode::Importing => {
                DirectoryMode::Retired
            }
        };
        state.version = state
            .version
            .checked_add(1)
            .ok_or(Error::Command("directory version overflow"))?;
        state.mode = mode.clone();
        save(context, &state)?;
        Ok(CommandResult::Success(Json(Some(mode))))
    }
}

/// Durable child retirement receipt addressed to its immutable parent generation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectoryRetirementReceipt {
    pub parent: DirectorySpec,
    pub child_id: [u8; 16],
    pub sequence: u64,
}

/// Acknowledge one retired child without losing the other child's recovery address.
pub struct RecordDirectoryRetirement;
impl Command for RecordDirectoryRetirement {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 9;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<DirectoryRetirementReceipt>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let Some(mut state) = state(|batch| context.sql(batch))? else {
            return Ok(CommandResult::Rejected(Json(false)));
        };
        if state.spec != input.parent || input.sequence == 0 || input.sequence > i64::MAX as u64 {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        let (children, mut acknowledged) = match state.mode {
            DirectoryMode::Retiring {
                children,
                acknowledged,
            } => (children, acknowledged),
            DirectoryMode::Retired => return Ok(CommandResult::Success(Json(true))),
            _ => return Ok(CommandResult::Rejected(Json(false))),
        };
        let Some(position) = children
            .iter()
            .position(|child| child.node_id == input.child_id)
        else {
            return Ok(CommandResult::Rejected(Json(false)));
        };
        acknowledged |= 1 << position;
        let retired = acknowledged == 3;
        // Keep a terminal fence after dropping child references. Reinstall,
        // opening and delayed range publication must not resurrect this node.
        state.mode = if retired {
            DirectoryMode::Retired
        } else {
            DirectoryMode::Retiring {
                children,
                acknowledged,
            }
        };
        save(context, &state)?;
        Ok(CommandResult::Success(Json(retired)))
    }
}
