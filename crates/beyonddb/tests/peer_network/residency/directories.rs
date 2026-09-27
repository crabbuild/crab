use super::provisioning::{Remote, wait_for_expiry};
use super::*;
use beyonddb::{
    DirectoryInstall, DirectoryMode, DirectoryPage, DirectoryPageInput, DirectorySpec,
    InstallDirectory, ReadDirectory, ReadDirectoryPage, RoutePagePartition, directory_target,
};
use crab_cell_runtime::{MutationIdentity, cell::catalog::CellCatalog, identity::RequestId};
use std::time::Duration;

fn mutation() -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes()),
        issued_at_ms: now_ms(),
        expires_at_ms: now_ms() + 60_000,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directory_split_and_retirement_follow_remote_owners() {
    const ACCOUNT: &str = "123456789012";
    let fixture = Fixture::new().await;
    let table = fixture
        .client
        .query::<DescribeTable>(
            &account_target(ACCOUNT).unwrap(),
            None,
            Json("Residency".into()),
        )
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let root = DirectorySpec {
        table_id: table.id,
        node_id: [0; 16],
        lower: [0; 16],
        upper: None,
        depth: 0,
    };
    let target = directory_target(ACCOUNT, &root).unwrap();
    fixture
        .provisioner
        .admit_directory(ACCOUNT, &root)
        .await
        .unwrap();
    let ranges = (0_u128..4)
        .map(|position| RoutePagePartition {
            partition_id: position.to_be_bytes(),
            lower: (position << 126).to_be_bytes(),
            upper: (position < 3).then(|| ((position + 1) << 126).to_be_bytes()),
            epoch: 1,
        })
        .collect();
    fixture
        .client
        .command::<InstallDirectory>(
            &target,
            mutation(),
            Json(DirectoryInstall {
                spec: root.clone(),
                ranges,
                source: None,
            }),
        )
        .await
        .unwrap();
    let remote = Remote::new(&fixture).await;
    // Use real signed capacity to place the copies on the empty node. The
    // controller must create, copy and open through authenticated admission.
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let node = fixture
                .directory
                .load(fixture.session, now_ms())
                .await
                .unwrap()
                .unwrap();
            if node
                .advertisement()
                .placement_capacity()
                .unwrap()
                .active_cells
                >= 5
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let split = fixture
        .provisioner
        .split_directory(&fixture.client, ACCOUNT, &root)
        .await
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    for child in &split.children {
        let target = directory_target(ACCOUNT, child).unwrap();
        let control = authority.load(target.cell_id()).await.unwrap().unwrap();
        assert_eq!(
            control.value().owner.as_ref().unwrap().session,
            remote.session
        );
    }
    // Releasing a copied child must restore its published contents through the
    // normal peer resolver, without the controller re-installing the copy.
    let child = &split.children[0];
    let child_target = directory_target(ACCOUNT, child).unwrap();
    let proof = CellCatalog::new(fixture.layout.clone(), child_target.tenant())
        .lookup(child_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let control = authority
        .load(child_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    remote
        .node
        .runtime()
        .local_handle(proof, &control)
        .await
        .unwrap()
        .unwrap()
        .drain()
        .await
        .unwrap();
    let page = fixture
        .client
        .query::<ReadDirectoryPage>(
            &child_target,
            None,
            Json(DirectoryPageInput {
                hash: child.lower,
                expected_version: Some(1),
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert!(matches!(page, DirectoryPage::Leaf { ranges, .. } if ranges.len() == 2));
    assert_eq!(
        authority
            .load(child_target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .as_ref()
            .unwrap()
            .session,
        remote.session
    );

    // The first step must retire a live remote child and acknowledge it locally.
    assert!(
        !fixture
            .provisioner
            .retire_directory_step(&fixture.client, ACCOUNT, &root)
            .await
            .unwrap()
    );
    assert!(matches!(
        fixture
            .client
            .query::<ReadDirectory>(&target, None, Json(()))
            .await
            .unwrap()
            .output
            .0
            .unwrap()
            .mode,
        DirectoryMode::Retiring {
            acknowledged: 1,
            ..
        }
    ));
    // Drop the other child's receipt, then lose its owner. Recovery must observe
    // the retained terminal fence and finish the parent without another copy.
    let second = directory_target(ACCOUNT, &split.children[1]).unwrap();
    fixture
        .client
        .command::<beyonddb::RetireDirectory>(&second, mutation(), Json(split.children[1].clone()))
        .await
        .unwrap();
    let former = remote.session;
    remote.shutdown().await;
    // This fixture stops the runtime without the server's session-retirement
    // hook. Its last signed advertisement remains eligible until lease expiry.
    wait_for_expiry(&fixture, former).await;
    assert!(
        fixture
            .provisioner
            .retire_directory_step(&fixture.client, ACCOUNT, &root)
            .await
            .unwrap()
    );
    for spec in std::iter::once(&root).chain(split.children.iter()) {
        let target = directory_target(ACCOUNT, spec).unwrap();
        assert_eq!(
            fixture
                .client
                .query::<ReadDirectory>(&target, None, Json(()))
                .await
                .unwrap()
                .output
                .0
                .unwrap()
                .mode,
            DirectoryMode::Retired
        );
    }
    fixture.shutdown().await;
}
