use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_cold_placement_activates_remote_and_resumes_its_claim() {
    use crab_cell_runtime::{
        control::{ControlState, Owner, Transition},
        fleet::placement::PlacementPlanner,
        identity::CellTarget,
        peer::{PeerOperation, decode_peer_reply, wire},
    };

    let fixture = Fixture::new().await;
    let sdk = aws_sdk_dynamodb::Client::from_conf(
        fixture
            .sdk
            .config()
            .to_builder()
            .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
            .build(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("https://{}", listener.local_addr().unwrap());
    let session = SessionId::from_bytes([96; 16]);
    let application = &fixture.application;
    let (remote, _tasks) = start_node(
        application.clone(),
        fixture.directory.clone(),
        session,
        endpoint.clone(),
        fixture.remote_tls.certificate(),
        fixture.remote_tls.signing_key().clone(),
        98,
        CancellationToken::new(),
    )
    .await;
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            remote.runtime(),
            application.clone(),
            fixture.layout.clone(),
            session,
            endpoint.clone(),
            fixture._files.path().join("remote-data"),
        )
        .unwrap(),
    );
    let peers = BeyonddbPeers::new(
        &remote,
        fixture.layout.clone(),
        fixture.directory.clone(),
        session,
        &fixture.remote_tls,
    )
    .unwrap();
    let router = peers.router(provisioner.clone());
    let tls = LoadedPeerTls::load(
        &fixture._files.path().join("remote.crt"),
        &fixture._files.path().join("remote.key"),
        &fixture._files.path().join("ca.crt"),
        "localhost",
    )
    .unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            tls.listener(listener),
            router.into_make_service_with_connect_info::<PeerTlsIdentity>(),
        )
        .await
        .unwrap();
    });
    let target_for = |entry: &crab_cell_runtime::cell::catalog::CatalogEntry| {
        CellTarget::new(
            account_target("123456789012").unwrap().tenant(),
            beyonddb::APPLICATION_ID,
            entry.namespace(),
            entry.partition(),
        )
        .unwrap()
    };
    let entries = fixture
        .node
        .runtime()
        .active_catalog_entries()
        .await
        .unwrap();
    let target = target_for(
        entries
            .iter()
            .find(|entry| entry.cell() == fixture.data[0].0.cell_id())
            .unwrap(),
    );
    let authority = CellAuthority::new(fixture.layout.clone());
    let transport = Arc::new(PeerHttpRoundTrip::new(
        Arc::new(BeyonddbPeerScope),
        authority.clone(),
        fixture.directory.clone(),
        Arc::new(fixture.remote_tls.client_identity()),
        session,
    ));
    let signer = Arc::new(PeerSigner::new(
        session,
        application.registry().release_digest(),
        fixture.remote_tls.signing_key().clone(),
    ));
    let principal = PeerPrincipal {
        issuer: format!(
            "beyonddb-peer:{}",
            fixture
                .directory
                .fleet()
                .as_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ),
        subject: session
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        actions: Vec::new(),
    };
    let handle = &fixture.data[0].0;
    let expected = wire::CellDescription {
        cell_id: handle.cell_id().as_bytes().to_vec(),
        incarnation: handle.incarnation().as_bytes().to_vec(),
        code: handle.code().as_bytes().to_vec(),
        schema: handle.schema(),
    };
    let send = |target: CellTarget, action: &str, describe: bool| {
        let transport = transport.clone();
        let signer = signer.clone();
        let directory = fixture.directory.clone();
        let destination = fixture.session;
        let expected = (!describe).then(|| expected.clone());
        let mut principal = principal.clone();
        principal.actions.push(action.to_owned());
        async move {
            let node = directory
                .load(destination, now_ms())
                .await?
                .ok_or(crab_cell_runtime::Error::CellNotActive)?
                .advertisement()
                .clone();
            let now = now_ms();
            let request = signer
                .sign(
                    principal,
                    now,
                    now + 60_000,
                    30_000,
                    PeerOperation::Read(wire::ReadRequest {
                        target: Some(wire::Target {
                            tenant_id: target.tenant().as_bytes().to_vec(),
                            application_id: target.application().as_bytes().to_vec(),
                            namespace_id: target.namespace().as_bytes().to_vec(),
                            partition: target.partition().to_vec(),
                        }),
                        timeout_ms: 30_000,
                        minimum: None,
                        expected,
                        operation: Some(if describe {
                            wire::read_request::Operation::Describe(true)
                        } else {
                            wire::read_request::Operation::CellQuery(wire::CellQuery {
                                query_id: 1,
                                codec_version: 1,
                                input: b"null".to_vec(),
                            })
                        }),
                    }),
                )
                .unwrap();
            let reply = transport
                .send_to_node(target, node, request, 30_000)
                .await?;
            Ok::<_, crab_cell_runtime::Error>(decode_peer_reply(&reply)?.outcome.unwrap())
        }
    };
    fixture.data[0].0.drain().await.unwrap();
    for (action, describe, code) in [
        ("beyonddb.cell.invoke", true, wire::error::Code::Unavailable),
        (
            "beyonddb.cell.activate",
            false,
            wire::error::Code::PermissionDenied,
        ),
        ("cell.activate", true, wire::error::Code::PermissionDenied),
        (
            "beyonddb.cell.provision",
            false,
            wire::error::Code::PermissionDenied,
        ),
    ] {
        let reply = send(target.clone(), action, describe).await;
        if code == wire::error::Code::Unavailable {
            assert!(matches!(
                reply,
                Err(crab_cell_runtime::Error::CellNotActive)
            ));
        } else {
            assert!(
                matches!(reply.unwrap(), wire::peer_reply::Outcome::Error(error) if error.code == code as i32)
            );
        }
        assert_eq!(
            authority
                .load(target.cell_id())
                .await
                .unwrap()
                .unwrap()
                .value()
                .state,
            ControlState::Idle
        );
    }
    let account = fixture
        .provisioner
        .admit_account("123456789012")
        .await
        .unwrap();
    account.drain().await.unwrap();
    for action in ["beyonddb.cell.activate", "beyonddb.cell.provision"] {
        let reply = send(account_target("123456789012").unwrap(), action, true).await;
        assert!(
            matches!(reply.unwrap(), wire::peer_reply::Outcome::Error(error) if error.code == wire::error::Code::PermissionDenied as i32)
        );
    }
    let unknown = CellTarget::new(
        target.tenant(),
        target.application(),
        target.namespace(),
        b"uncataloged",
    )
    .unwrap();
    let reply = send(unknown.clone(), "beyonddb.cell.provision", true).await;
    assert!(matches!(
        reply,
        Err(crab_cell_runtime::Error::CellNotActive)
    ));
    assert!(authority.load(unknown.cell_id()).await.unwrap().is_none());
    assert_eq!(
        authority
            .load(account.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .state,
        ControlState::Idle
    );
    fixture
        .provisioner
        .admit_account("123456789012")
        .await
        .unwrap();

    // Wait for fresh measured advertisements to favor the empty node. The SDK
    // request itself must perform activation; the test never assigns its owner.
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let selected = fixture
                .directory
                .choose_advertised_placement(
                    &PlacementPlanner::default(),
                    target.cell_id(),
                    now_ms(),
                    4,
                )
                .await
                .unwrap();
            if selected.is_some_and(|selected| selected.session == session) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let expected = &fixture.data[0].1;
    let read = sdk
        .get_item()
        .table_name("Residency")
        .key("id", expected["id"].clone())
        .consistent_read(true)
        .send()
        .await
        .unwrap();
    assert_eq!(read.item.as_ref(), Some(expected));
    assert_eq!(
        authority
            .load(target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .as_ref()
            .unwrap()
            .session,
        session
    );
    // An activation sent to the old host cannot steal a live remote owner.
    for action in ["beyonddb.cell.activate", "beyonddb.cell.provision"] {
        let reply = send(target.clone(), action, true).await;
        assert!(matches!(
            reply,
            Err(crab_cell_runtime::Error::CellNotActive)
        ));
    }
    assert_eq!(
        authority
            .load(target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .as_ref()
            .unwrap()
            .session,
        session
    );

    let second = target_for(
        entries
            .iter()
            .find(|entry| entry.cell() == fixture.data[1].0.cell_id())
            .unwrap(),
    );
    fixture.data[1].0.drain().await.unwrap();
    let idle = authority.load(second.cell_id()).await.unwrap().unwrap();
    let claimed = idle.value().takeover(Owner { session, endpoint }).unwrap();
    authority
        .transition(&idle, claimed, Transition::Takeover)
        .await
        .unwrap();
    // Model loss after the claim CAS, before actor admission. The original
    // ingress resumes on that remote owner instead of choosing a new one.
    let expected = &fixture.data[1].1;
    let read = sdk
        .get_item()
        .table_name("Residency")
        .key("id", expected["id"].clone())
        .consistent_read(true)
        .send()
        .await
        .unwrap();
    assert_eq!(read.item.as_ref(), Some(expected));
    assert_eq!(
        authority
            .load(second.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .as_ref()
            .unwrap()
            .session,
        session
    );
    let writes = fixture
        .data
        .iter()
        .map(|(_, item)| {
            aws_sdk_dynamodb::types::TransactWriteItem::builder()
                .update(
                    aws_sdk_dynamodb::types::Update::builder()
                        .table_name("Residency")
                        .key("id", item["id"].clone())
                        .update_expression("SET #v = :after")
                        .condition_expression("#v = :before")
                        .expression_attribute_names("#v", "value")
                        .expression_attribute_values(
                            ":before",
                            AwsAttributeValue::S("committed".into()),
                        )
                        .expression_attribute_values(
                            ":after",
                            AwsAttributeValue::S("remote transaction".into()),
                        )
                        .build()
                        .unwrap(),
                )
                .build()
        })
        .collect();
    sdk.transact_write_items()
        .client_request_token("cold-remote-transaction")
        .set_transact_items(Some(writes))
        .send()
        .await
        .unwrap();
    let account = account_target("123456789012").unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("Residency".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let before = fixture
        .client
        .query::<ReadTableRoute>(&account, None, Json(table.id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    fixture
        .provisioner
        .install_account_capacity_loop(
            &fixture.tasks,
            "123456789012".into(),
            fixture.client.clone(),
            1,
            std::time::Duration::from_secs(3_600),
        )
        .unwrap();
    // The supervised loop's first tick must split the remotely owned range.
    // Wait for publication and both child opens before moving one child again.
    let split = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            assert!(
                fixture.node.is_ready(),
                "remote capacity work stopped serving"
            );
            let route = fixture
                .client
                .query::<ReadTableRoute>(&account, None, Json(table.id.clone()))
                .await
                .unwrap()
                .output
                .0
                .unwrap();
            if route.epoch == before.epoch + 1 {
                let split = fixture
                    .provisioner
                    .split_if_over_database_bytes(
                        "123456789012",
                        fixture.client.clone(),
                        &table.id,
                        before.partitions[0].partition_id,
                        1,
                    )
                    .await
                    .unwrap()
                    .unwrap();
                let mut opened = true;
                for child in &split.children {
                    let target =
                        beyonddb::data_target("123456789012", &child.table.id, &child.partition_id)
                            .unwrap();
                    let status = fixture
                        .client
                        .query::<beyonddb::ReadPartitionState>(&target, None, Json(()))
                        .await
                        .unwrap()
                        .output
                        .0
                        .unwrap();
                    opened &= matches!(status.state, beyonddb::PartitionState::Opened { .. });
                }
                if opened {
                    break split;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        authority
            .load(
                beyonddb::data_target(
                    "123456789012",
                    &split.source.table.id,
                    &split.source.partition_id
                )
                .unwrap()
                .cell_id()
            )
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .as_ref()
            .unwrap()
            .session,
        session
    );
    let key = Item::from([(
        "id".into(),
        AttributeValue::S(fixture.data[0].1["id"].as_s().unwrap().clone()),
    )]);
    let hash =
        beyonddb::data_key_hash(&split.source.table.id, &key, &split.source.table.key_schema)
            .unwrap();
    let mut child = split
        .children
        .iter()
        .find(|child| {
            child.lower.is_none_or(|lower| hash >= lower)
                && child.upper.is_none_or(|upper| hash < upper)
        })
        .unwrap();
    let mut already_remote = false;
    for candidate in &split.children {
        let target =
            beyonddb::data_target("123456789012", &candidate.table.id, &candidate.partition_id)
                .unwrap();
        if authority
            .load(target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .as_ref()
            .unwrap()
            .session
            == session
        {
            child = candidate;
            already_remote = true;
            break;
        }
    }
    let child_target =
        beyonddb::data_target("123456789012", &child.table.id, &child.partition_id).unwrap();
    if !already_remote {
        // If initial placement chose both children locally, moving this one
        // exercises completed-plan replay after ownership changes as before.
        fixture
            .provisioner
            .admit_existing_partition("123456789012", &child.table.id, &child.partition_id)
            .await
            .unwrap()
            .drain()
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let selected = fixture
                    .directory
                    .choose_advertised_placement(
                        &PlacementPlanner::default(),
                        child_target.cell_id(),
                        now_ms(),
                        4,
                    )
                    .await
                    .unwrap();
                if selected.is_some_and(|selected| selected.session == session) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    }
    let read = sdk
        .get_item()
        .table_name("Residency")
        .key("id", fixture.data[0].1["id"].clone())
        .consistent_read(true)
        .send()
        .await
        .unwrap();
    assert_eq!(
        read.item.unwrap()["value"],
        AwsAttributeValue::S("remote transaction".into())
    );
    // A completed plan preserves remote ownership, whether the child was
    // initially placed there or moved after opening.
    fixture
        .provisioner
        .resume_split("123456789012", fixture.client.clone(), &split)
        .await
        .unwrap();
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
        session
    );
    let completed = fixture
        .provisioner
        .split_if_over_database_bytes(
            "123456789012",
            fixture.client.clone(),
            &split.source.table.id,
            split.source.partition_id,
            1,
        )
        .await
        .unwrap();
    assert_eq!(completed, Some(split));
    remote.shutdown().await.unwrap();
    server.abort();
    let _ = server.await;
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while fixture.directory.is_live(session, now_ms()).await.unwrap() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    // Graceful owner removal publishes roots. After lease expiry, SDK reads restore
    // both changed participants on the remaining node without manual placement.
    for (_, item) in &fixture.data {
        let mut expected = item.clone();
        expected.insert(
            "value".into(),
            AwsAttributeValue::S("remote transaction".into()),
        );
        let read = sdk
            .get_item()
            .table_name("Residency")
            .key("id", item["id"].clone())
            .consistent_read(true)
            .send()
            .await
            .unwrap();
        assert_eq!(read.item, Some(expected));
    }
    fixture.shutdown().await;
}
