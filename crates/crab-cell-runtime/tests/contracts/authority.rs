//! Authority CAS tests.
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::control::{Control, Transition};
use crab_cell_runtime::ltx::CellStorageLayout;

use std::sync::Arc;

use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};

use bytes::Bytes;
use crab_cell_runtime::*;

use crab_cell_runtime::control::{ControlState, Owner, RootRef};
use crab_cell_runtime::identity::IncarnationId;
use crab_cell_runtime::identity::{Digest, SessionId};

fn control() -> Control {
    Control::initial(
        CellId::from_bytes([1; 32]),
        IncarnationId::from_bytes([2; 16]),
        Owner {
            session: SessionId::from_bytes([3; 16]),
            endpoint: "https://node.internal:8081".into(),
        },
        Digest::from_bytes([4; 32]),
        1,
    )
    .unwrap()
}

#[tokio::test]
async fn stale_control_token_cannot_overwrite_winning_publication() {
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(store, Path::from("root"), [8; 16]);
    let authority = CellAuthority::new(layout.clone());
    let initial = control();
    layout
        .store()
        .create_strict(
            &layout.control_path(initial.cell.as_bytes()),
            Bytes::from(initial.encode().unwrap()),
        )
        .await
        .unwrap();
    let first = authority.load(initial.cell).await.unwrap().unwrap();
    let stale = authority.load(initial.cell).await.unwrap().unwrap();

    let mut published = initial.clone();
    published.revision += 1;
    published.progress += 1;
    published.state = ControlState::Serving;
    published.root = Some(RootRef {
        digest: Digest::from_bytes([9; 32]),
        txid: 1,
        checksum: (1 << 63) | 7,
        commit_sequence: 1,
    });
    let winner = authority
        .transition(&first, published.clone(), Transition::Publish)
        .await
        .unwrap();
    assert_eq!(winner.value(), &published);

    let mut stale_publish = initial;
    stale_publish.revision += 1;
    stale_publish.progress += 1;
    stale_publish.state = ControlState::Serving;
    stale_publish.root = Some(RootRef {
        digest: Digest::from_bytes([5; 32]),
        txid: 1,
        checksum: (1 << 63) | 8,
        commit_sequence: 1,
    });
    assert!(
        authority
            .transition(&stale, stale_publish, Transition::Publish)
            .await
            .is_err()
    );
    assert_eq!(
        authority
            .load(published.cell)
            .await
            .unwrap()
            .unwrap()
            .value(),
        &published
    );
}
