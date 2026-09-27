//! Independent entity ledgers reached through every public host and the balancer.

use super::super::fleet::{
    GatewayStats, balancer_round_trip, start_balancer, start_gateway_peer_server,
};
use super::super::performance_fixture::{install_sql_tables, node_session, now_ms, rustfs_store};
use super::super::process_node;
use super::*;
use crab_cell_runtime::cell::catalog::CellCatalog;
use crab_cell_runtime::cell::executor::Resolution;
use crab_cell_runtime::peer::PeerVerifier;
use futures_util::future::join_all;
use tokio::net::TcpListener;

const NODES: usize = 3;
const ENTITIES_PER_NODE: usize = 4;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn entity_ledgers_are_isolated_across_three_public_hosts() {
    run(Store::new(Arc::new(InMemory::new())), "entity-hosts".into()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real RustFS endpoint and unique CRAB_CELL_TEST_PREFIX required"]
async fn entity_ledgers_are_isolated_across_three_rustfs_hosts() {
    let root = std::env::var("CRAB_CELL_TEST_PREFIX").unwrap();
    run(rustfs_store(), root.into()).await;
}

async fn run(store: Store, root: object_store::path::Path) {
    let application = compiled_entities();
    let registry = application.registry();
    let tenant = TenantId::from_bytes([81; 16]);
    let application_id = ApplicationId::from_bytes([82; 16]);
    let layout = CellStorageLayout::new(store, root, *application_id.as_bytes());
    let catalog = CellCatalog::new(layout.clone(), tenant);
    let authority = CellAuthority::new(layout.clone());
    let directory = tempfile::TempDir::new().unwrap();
    let node_directories = (0..NODES)
        .map(|node| {
            let path = directory.path().join(format!("node-{node}"));
            std::fs::create_dir(&path).unwrap();
            path
        })
        .collect::<Vec<_>>();
    let mut listeners = Vec::new();
    for _ in 0..NODES {
        listeners.push(TcpListener::bind("127.0.0.1:0").await.unwrap());
    }
    let addresses = listeners
        .iter()
        .map(|listener| listener.local_addr().unwrap())
        .collect::<Vec<_>>();
    let nodes = join_all(addresses.iter().enumerate().map(|(node, address)| {
        process_node::start(
            node,
            application.clone(),
            &layout,
            &node_directories[node],
            format!("https://{address}"),
        )
    }))
    .await;

    // Provision from the descriptor independently of the generated accessor:
    // otherwise a shared routing error could make both sides agree on one Cell.
    let cell_type = application.cell_types()[0];
    let mut owned = vec![Vec::new(); NODES];
    let mut targets = Vec::new();
    let mut routes = HashMap::new();
    for entity in 0..NODES * ENTITIES_PER_NODE {
        let node = entity % NODES;
        let key = format!("order-{entity}");
        let target = CellTarget::new(
            tenant,
            application_id,
            SQL_NAMESPACE,
            &cell_type.entity_partition(key.as_bytes()).unwrap(),
        )
        .unwrap();
        let proof = catalog
            .provision(
                CatalogEntry::new(
                    &target,
                    cell_type.role(),
                    registry.module_code(cell_type.module()).unwrap(),
                    1,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let incarnation = IncarnationId::from_bytes([entity as u8 + 1; 16]);
        let observed = authority
            .create_initial(
                &proof,
                incarnation,
                Owner {
                    session: node_session(node),
                    endpoint: format!("https://{}", addresses[node]),
                },
            )
            .await
            .unwrap();
        let replica = CellReplica::new(
            layout.clone(),
            *target.cell_id().as_bytes(),
            *incarnation.as_bytes(),
            Limits {
                max_database_bytes: cell_type.database_limit_bytes(),
                max_capture_bytes: cell_type.capture_limit_bytes(),
                ..Limits::default()
            },
        )
        .unwrap();
        let path = node_directories[node].join(format!("order-{entity}.sqlite"));
        let handle = nodes[node]
            .0
            .runtime()
            .bootstrap(
                proof,
                replica,
                authority.clone(),
                observed,
                path,
                install_sql_tables,
            )
            .await
            .unwrap();
        owned[node].push(handle);
        routes.insert(target.cell_id(), addresses[node]);
        targets.push(target);
    }
    let signer = PeerSigner::new(
        crab_cell_runtime::SessionId::from_bytes([77; 16]),
        registry.release_digest(),
        SigningKey::from_bytes(&[78; 32]),
    );
    let verifier = Arc::new(PeerVerifier::new(
        crab_cell_runtime::SessionId::from_bytes([77; 16]),
        registry.release_digest(),
        signer.verifying_key(),
    ));
    let stats = (0..NODES)
        .map(|_| Arc::new(GatewayStats::default()))
        .collect::<Vec<_>>();
    let mut servers = Vec::new();
    for (node, listener) in listeners.into_iter().enumerate() {
        servers.push(start_gateway_peer_server(
            listener,
            &registry,
            verifier.clone(),
            owned[node].clone(),
            routes.clone(),
            stats[node].clone(),
            Some((
                Arc::new(nodes[node].2.clone()),
                process_node::directory(&layout, &registry),
            )),
        ));
    }
    let (address, balancer, ingress) = start_balancer(addresses).await;
    servers.push(balancer);
    let clients = nodes
        .iter()
        .map(|(host, _, _)| {
            let handle = host
                .application_handle::<EntityReferenceApplication>(
                    peer_client(&application, balancer_round_trip(address)),
                    tenant,
                    application_id,
                )
                .unwrap();
            EntityReferenceClient::new(handle).unwrap()
        })
        .collect::<Vec<_>>();

    // Reuse the same mutation identity and payload across different entities.
    // The durable request ledger must be scoped to the selected Cell.
    let mutation = reference_identity(115, now_ms());
    let input = CronInvocation {
        schedule_id: [116; 16],
        generation: 1,
        occurrence: 1,
        scheduled_at_ms: now_ms(),
        payload: b"entity-invoice".to_vec(),
    };
    let mut receipts = Vec::new();
    for (entity, target) in targets.iter().enumerate() {
        let client = &clients[entity % NODES];
        let order = client
            .orders(&OrderId(format!("order-{entity}").into_bytes()))
            .unwrap();
        assert_eq!(order.target(), target);
        let prepared = order
            .prepare_receive_cron(mutation, input.clone())
            .await
            .unwrap();
        let pending = prepared.evidence().clone();
        let first = prepared.execute().await.unwrap();
        assert!(matches!(
            client.resolve(&pending).await.unwrap(),
            Resolution::Committed(_)
        ));
        let replay = order.receive_cron(mutation, input.clone()).await.unwrap();
        assert_eq!(first.receipt, replay.receipt);
        receipts.push(first.receipt);
    }
    // Every host-bound author client must see every entity's independent state.
    for client in &clients {
        for (entity, receipt) in receipts.iter().enumerate() {
            let order = client
                .orders(&OrderId(format!("order-{entity}").into_bytes()))
                .unwrap();
            assert_eq!(
                order
                    .receipt_count(Some(*receipt), ())
                    .await
                    .unwrap()
                    .output,
                1
            );
        }
    }
    let mut owners = vec![0; NODES];
    for (target, receipt) in targets.iter().zip(&receipts) {
        let control = authority.load(target.cell_id()).await.unwrap().unwrap();
        let node = (0..NODES)
            .find(|node| control.value().owner.as_ref().unwrap().session == node_session(*node))
            .unwrap();
        owners[node] += 1;
        assert!(control.value().root.as_ref().unwrap().commit_sequence >= receipt.commit_sequence);
    }
    assert_eq!(owners, vec![ENTITIES_PER_NODE; NODES]);
    let ingress_counts = ingress.counts();
    assert!(ingress_counts.iter().all(|count| *count > 0));
    assert!(ingress_counts.iter().max().unwrap() - ingress_counts.iter().min().unwrap() <= 1);
    for counts in &stats {
        let (local, forwarded) = counts.counts();
        assert!(
            local > 0 && forwarded > 0,
            "local={local} forwarded={forwarded}"
        );
    }
    println!(
        "ENTITY_PROOF nodes={NODES} cells={} owners={owners:?} ingress={ingress_counts:?} visible_receipts={}",
        targets.len(),
        receipts.len()
    );
    for (node, _, _) in nodes {
        node.shutdown().await.unwrap();
    }
    assert!(
        process_node::directory(&layout, &registry)
            .live(now_ms(), 32)
            .await
            .unwrap()
            .is_empty()
    );
    for server in servers {
        server.abort();
        let _ = server.await;
    }
}
