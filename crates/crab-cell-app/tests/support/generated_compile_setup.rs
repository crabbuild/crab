use crab_cell_app::{ApplicationBuilder, CellApplication, CellKey, cell_client};
use crab_cell_runtime::identity::NamespaceId;
use crab_cell_runtime::registry::{Command, CommandContext, CommandResult};

struct App;

struct EntityKey([u8; 16]);

impl CellKey for EntityKey {
    fn canonical_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl CellApplication for App {
    const NAME: &'static str = "compile-proof";

    fn register(_builder: &mut ApplicationBuilder) -> crab_cell_runtime::Result<()> {
        Ok(())
    }
}

struct Set;

impl Command for Set {
    const MODULE: &'static str = "entity";
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = ();
    type Output = ();

    fn execute(
        _context: &mut CommandContext<'_, '_>,
        _input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        Ok(CommandResult::Success(()))
    }
}

cell_client! {
    struct Client (App) {
        fn entity(scope: &EntityKey) -> Entity {
            namespace: NamespaceId::from_bytes([1; 16]),
            module: "entity",
            commands: { fn set, prepare_set: Set = 1; },
            queries: { }
        }
    }
}
