use std::{
    env,
    path::{Path as FilePath, PathBuf},
    sync::Arc,
    time::Duration,
};

#[path = "../process_store.rs"]
mod process_store;

use crab_cell_runtime::{
    ApplicationId, CellAuthority, CellCatalog, CellRuntime, CellTarget, IncarnationId, NamespaceId,
    Owner, SessionId, SqlWorkerPool, TenantId,
};
use crab_ltx::CellStorageLayout;
use crab_ltx::{CellReplica, Limits};
use crab_storage::Store;
use object_store::path::Path;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let store_root = required(&mut args, "store root")?;
    let partition = required(&mut args, "partition")?;
    let session = decode_fixed::<16>(&required(&mut args, "session")?)?;
    let destination = PathBuf::from(required(&mut args, "destination")?);
    let hold_ms = required(&mut args, "hold milliseconds")?.parse::<u64>()?;
    let mode = match args.next().as_deref() {
        None | Some("drain") => ProbeMode::Drain,
        Some("crash") => ProbeMode::Crash,
        Some("lost-release") => ProbeMode::LostRelease,
        Some("fail-receiver") => ProbeMode::FailReceiver,
        Some(_) => {
            return Err("mode must be drain, crash, lost-release, or fail-receiver".into());
        }
    };
    if args.next().is_some() {
        return Err(
            "usage: cell_movement_probe <store-root> <partition> <session> <destination> <hold-ms> [drain|crash|lost-release|fail-receiver]"
                .into(),
        );
    }

    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        NamespaceId::from_bytes([6; 16]),
        partition.as_bytes(),
    )?;
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([2; 16]);
    let object_store = process_store::FilesystemCasStore::new(FilePath::new(&store_root))?;
    let shared_store = object_store.clone();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(shared_store)),
        Path::from("runtime"),
        [3; 16],
    );
    let catalog = CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .lookup(cell)
        .await?
        .ok_or("cell catalog entry is missing")?;
    let authority = CellAuthority::new(layout.clone());
    let observed = authority
        .load(cell)
        .await?
        .ok_or("cell control is missing")?;
    let replica = CellReplica::new(
        layout,
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )?;
    let session = SessionId::from_bytes(session);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1)?, 8 * 1024 * 1024, session)?;
    let owner = Owner {
        session,
        endpoint: "https://process-movement-probe.invalid".into(),
    };
    let acquisition = runtime
        .acquire_idle_restored(proof, replica, authority, observed, destination, owner)
        .await;
    if matches!(mode, ProbeMode::FailReceiver) {
        match acquisition {
            Ok(handle) => {
                handle.drain().await?;
                runtime.shutdown().await?;
                return Err("receiver-failure probe unexpectedly activated the Cell".into());
            }
            Err(_) => {
                runtime.shutdown().await?;
                return Ok(());
            }
        }
    }
    let handle = match acquisition {
        Ok(handle) => handle,
        Err(error) => {
            runtime.shutdown().await?;
            return Err(error.into());
        }
    };
    if matches!(mode, ProbeMode::LostRelease) {
        object_store.drop_next_update_response();
    }
    tokio::time::sleep(Duration::from_millis(hold_ms)).await;
    if matches!(mode, ProbeMode::Crash) {
        std::process::exit(0);
    }
    handle.drain().await?;
    if matches!(mode, ProbeMode::LostRelease) && !object_store.dropped_update_response() {
        return Err("lost-release probe did not inject a committed response loss".into());
    }
    runtime.shutdown().await?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ProbeMode {
    Drain,
    Crash,
    LostRelease,
    FailReceiver,
}

fn required(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("missing {name}"))
}

fn decode_fixed<const N: usize>(value: &str) -> Result<[u8; N], String> {
    if value.len() != N * 2 {
        return Err(format!("expected {} hexadecimal characters", N * 2));
    }
    let mut bytes = [0_u8; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (decode_nibble(pair[0])? << 4) | decode_nibble(pair[1])?;
    }
    Ok(bytes)
}

fn decode_nibble(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err("session must be hexadecimal".into()),
    }
}
