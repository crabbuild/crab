#![cfg(unix)]

mod support;

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    ops::{Deref, DerefMut},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use aws_credential_types::Credentials;
use aws_sdk_dynamodb::types::{
    AttributeValue, ConditionCheck, KeysAndAttributes, PutRequest, TimeToLiveSpecification,
    TransactWriteItem, Update, WriteRequest,
};
use serde_json::json;

struct ManagedChild(Child);

impl Deref for ManagedChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ManagedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn run(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn tls_files(root: &Path) {
    run(Command::new("openssl")
        .args(["genpkey", "-algorithm", "ED25519", "-out"])
        .arg(root.join("ca.key")));
    run(Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-new",
            "-days",
            "1",
            "-subj",
            "/CN=BeyondDB Test CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-key",
        ])
        .arg(root.join("ca.key"))
        .arg("-out")
        .arg(root.join("ca.crt")));
    run(Command::new("openssl")
        .args(["genpkey", "-algorithm", "ED25519", "-out"])
        .arg(root.join("peer.key")));
    run(Command::new("openssl")
        .args(["req", "-new", "-subj", "/CN=localhost", "-key"])
        .arg(root.join("peer.key"))
        .arg("-out")
        .arg(root.join("peer.csr")));
    fs::write(
        root.join("peer.ext"),
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth,clientAuth\nsubjectAltName=DNS:localhost\n",
    )
    .unwrap();
    run(Command::new("openssl")
        .args(["x509", "-req", "-days", "1", "-CAcreateserial", "-in"])
        .arg(root.join("peer.csr"))
        .arg("-CA")
        .arg(root.join("ca.crt"))
        .arg("-CAkey")
        .arg(root.join("ca.key"))
        .arg("-extfile")
        .arg(root.join("peer.ext"))
        .arg("-out")
        .arg(root.join("peer.crt")));
}

fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn start(config: &Path, log: &Path, bootstrap: bool, s3: SocketAddr) -> ManagedChild {
    let output = File::create(log).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_beyonddb"));
    command
        .arg(config)
        .stderr(output)
        .stdout(Stdio::null())
        .env("AWS_ACCESS_KEY_ID", "crab")
        .env("AWS_SECRET_ACCESS_KEY", "crab")
        .env("AWS_REGION", "us-east-1")
        .env("AWS_ALLOW_HTTP", "true")
        .env("AWS_ENDPOINT_URL_S3", format!("http://{s3}"))
        .env("AWS_VIRTUAL_HOSTED_STYLE_REQUEST", "false");
    if bootstrap {
        command.arg("--bootstrap").stdin(Stdio::piped());
    }
    let mut child = command.spawn().unwrap();
    if bootstrap {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n")
            .unwrap();
    }
    ManagedChild(child)
}

fn wait_healthy(child: &mut Child, address: SocketAddr, log: &Path) {
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "server exited {status}: {}",
                fs::read_to_string(log).unwrap()
            );
        }
        if let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(250)) {
            stream
                .set_read_timeout(Some(Duration::from_millis(250)))
                .unwrap();
            stream
                .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .unwrap();
            let mut response = String::new();
            if stream.read_to_string(&mut response).is_ok() && response.contains("200 OK") {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "server did not become healthy: {}",
        fs::read_to_string(log).unwrap()
    );
}

fn stop(child: &mut Child, log: &Path) {
    run(Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string()));
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{}", fs::read_to_string(log).unwrap());
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    child.kill().unwrap();
    panic!("server did not stop: {}", fs::read_to_string(log).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires local rustfs and aws CLI"]
async fn bootstrap_sdk_write_survives_unclean_server_restart() {
    let root = tempfile::tempdir().unwrap();
    tls_files(root.path());
    fs::create_dir(root.path().join("objects")).unwrap();
    let s3 = free_addr();
    let rustfs = Command::new("rustfs")
        .arg("server")
        .arg("--address")
        .arg(s3.to_string())
        .arg(root.path().join("objects"))
        .env("RUSTFS_ACCESS_KEY", "crab")
        .env("RUSTFS_SECRET_KEY", "crab")
        .stdout(Stdio::null())
        .stderr(File::create(root.path().join("rustfs.log")).unwrap())
        .spawn()
        .unwrap();
    let mut rustfs = ManagedChild(rustfs);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let ready = Command::new("aws")
            .args([
                "--endpoint-url",
                &format!("http://{s3}"),
                "s3api",
                "list-buckets",
            ])
            .env("AWS_ACCESS_KEY_ID", "crab")
            .env("AWS_SECRET_ACCESS_KEY", "crab")
            .env("AWS_DEFAULT_REGION", "us-east-1")
            .output()
            .unwrap();
        if ready.status.success() {
            break;
        }
        assert!(
            rustfs.try_wait().unwrap().is_none(),
            "RustFS exited before S3 readiness"
        );
        assert!(Instant::now() < deadline, "RustFS did not become S3 ready");
        std::thread::sleep(Duration::from_millis(100));
    }
    run(Command::new("aws")
        .args([
            "--endpoint-url",
            &format!("http://{s3}"),
            "s3api",
            "create-bucket",
            "--bucket",
            "beyonddb-test",
        ])
        .env("AWS_ACCESS_KEY_ID", "crab")
        .env("AWS_SECRET_ACCESS_KEY", "crab")
        .env("AWS_DEFAULT_REGION", "us-east-1"));
    fs::write(root.path().join("encryption.key"), [23; 32]).unwrap();
    fs::write(
        root.path().join("policy.json"),
        json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Action": "dynamodb:*",
                "Resource": [
                    "arn:aws:dynamodb:us-east-1:123456789012:table/ProcessData",
                    "arn:aws:dynamodb:us-east-1:123456789012:table/*"
                ]
            }]
        })
        .to_string(),
    )
    .unwrap();
    let peer = free_addr();
    let public = free_addr();
    let config = root.path().join("config.json");
    fs::write(
        &config,
        json!({
            "storage_url": "s3://beyonddb-test/beyonddb",
            "node_id": "01994f26-5966-7b20-8b58-2fddf198a321",
            "data_dir": root.path().join("data"),
            "disk_budget_bytes": 1073741824,
            "encryption_key_file": root.path().join("encryption.key"),
            "region": "us-east-1",
            "peer_bind": peer,
            "peer_endpoint": format!("https://{peer}"),
            "peer_certificate": root.path().join("peer.crt"),
            "peer_private_key": root.path().join("peer.key"),
            "peer_ca": root.path().join("ca.crt"),
            "peer_server_name": "localhost",
            "public_bind": public,
            "public_endpoint": format!("http://{public}"),
            "owned_accounts": ["123456789012"],
            "owned_access_keys": ["AKIAIOSFODNN7EXAMPLE"],
            "initial_partitions": 4,
            "bootstrap": {
                "account_id": "123456789012",
                "access_key_id": "AKIAIOSFODNN7EXAMPLE",
                "principal_name": "process-user",
                "policy_name": "tables",
                "policy_file": root.path().join("policy.json")
            }
        })
        .to_string(),
    )
    .unwrap();
    let log = root.path().join("server.log");
    let mut child = start(&config, &log, true, s3);
    wait_healthy(&mut child, public, &log);
    let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            None,
            None,
            "beyonddb-process-test",
        ))
        .endpoint_url(format!("http://{public}"))
        .load()
        .await;
    let sdk = aws_sdk_dynamodb::Client::new(&sdk_config);
    let created = sdk
        .create_table()
        .table_name("ProcessData")
        .key_schema(
            aws_sdk_dynamodb::types::KeySchemaElement::builder()
                .attribute_name("id")
                .key_type(aws_sdk_dynamodb::types::KeyType::Hash)
                .build()
                .unwrap(),
        )
        .attribute_definitions(
            aws_sdk_dynamodb::types::AttributeDefinition::builder()
                .attribute_name("id")
                .attribute_type(aws_sdk_dynamodb::types::ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .billing_mode(aws_sdk_dynamodb::types::BillingMode::PayPerRequest)
        .tags(
            aws_sdk_dynamodb::types::Tag::builder()
                .key("created")
                .value("yes")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let arn = created
        .table_description()
        .and_then(|description| description.table_arn())
        .unwrap();
    let table_id = created
        .table_description()
        .and_then(|description| description.table_id())
        .unwrap();
    let schema = [extenddb_core::types::KeySchemaElement {
        attribute_name: "id".into(),
        key_type: extenddb_core::types::KeyType::Hash,
    }];
    let range = |id: &str| {
        beyonddb::data_key_hash(
            table_id,
            &extenddb_core::types::Item::from([(
                "id".into(),
                extenddb_core::types::AttributeValue::S(id.into()),
            )]),
            &schema,
        )
        .unwrap()[0]
            >> 6
    };
    let condition_key = (0..32)
        .map(|index| format!("condition-{index}"))
        .find(|id| range(id) != range("process"))
        .unwrap();
    let second_write_key = (0..32)
        .map(|index| format!("transaction-{index}"))
        .find(|id| range(id) != range("process"))
        .unwrap();
    let initial_tags = sdk
        .list_tags_of_resource()
        .resource_arn(arn)
        .send()
        .await
        .unwrap();
    assert_eq!(initial_tags.tags().len(), 1);
    assert_eq!(initial_tags.tags()[0].key(), "created");
    sdk.tag_resource()
        .resource_arn(arn)
        .tags(
            aws_sdk_dynamodb::types::Tag::builder()
                .key("team")
                .value("crab")
                .build()
                .unwrap(),
        )
        .tags(
            aws_sdk_dynamodb::types::Tag::builder()
                .key("env")
                .value("test")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let tagged = sdk
        .list_tags_of_resource()
        .resource_arn(arn)
        .send()
        .await
        .unwrap();
    assert_eq!(tagged.tags().len(), 3);
    sdk.tag_resource()
        .resource_arn(arn)
        .tags(
            aws_sdk_dynamodb::types::Tag::builder()
                .key("team")
                .value("elastic")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    sdk.untag_resource()
        .resource_arn(arn)
        .tag_keys("env")
        .send()
        .await
        .unwrap();
    let item = HashMap::from([
        ("id".into(), AttributeValue::S("process".into())),
        ("value".into(), AttributeValue::S("committed".into())),
    ]);
    sdk.put_item()
        .table_name("ProcessData")
        .set_item(Some(item.clone()))
        .send()
        .await
        .unwrap();
    let mut batch_ids: Vec<String> = Vec::new();
    for index in 0..64 {
        let id = format!("batch-{index}");
        if batch_ids
            .iter()
            .any(|existing| range(existing) == range(&id))
        {
            continue;
        }
        batch_ids.push(id);
        if batch_ids.len() == 3 {
            break;
        }
    }
    assert_eq!(batch_ids.len(), 3);
    let batch_items = batch_ids
        .iter()
        .map(|id| {
            HashMap::from([
                ("id".into(), AttributeValue::S(id.clone())),
                ("value".into(), AttributeValue::S("batched".into())),
            ])
        })
        .collect::<Vec<_>>();
    let writes = batch_items
        .iter()
        .map(|batch_item| {
            WriteRequest::builder()
                .put_request(
                    PutRequest::builder()
                        .set_item(Some(batch_item.clone()))
                        .build()
                        .unwrap(),
                )
                .build()
        })
        .collect();
    let written = sdk
        .batch_write_item()
        .request_items("ProcessData", writes)
        .send()
        .await
        .unwrap();
    assert!(written.unprocessed_items().is_none_or(HashMap::is_empty));
    let batch_keys = batch_items
        .iter()
        .map(|item| HashMap::from([("id".into(), item["id"].clone())]))
        .collect::<Vec<_>>();
    let read_batch = || {
        sdk.batch_get_item().request_items(
            "ProcessData",
            KeysAndAttributes::builder()
                .set_keys(Some(batch_keys.clone()))
                .consistent_read(true)
                .build()
                .unwrap(),
        )
    };
    let batch = read_batch().send().await.unwrap();
    let found = &batch.responses().unwrap()["ProcessData"];
    assert!(batch_items.iter().all(|item| found.contains(item)));
    for total_segments in [2, 3] {
        let mut scanned = HashSet::new();
        for segment in 0..total_segments {
            let mut start = None;
            for page_number in 0..20 {
                let page = sdk
                    .scan()
                    .table_name("ProcessData")
                    .segment(segment)
                    .total_segments(total_segments)
                    .limit(2)
                    .set_exclusive_start_key(start)
                    .send()
                    .await
                    .unwrap();
                for item in page.items() {
                    assert!(scanned.insert(item["id"].as_s().unwrap().clone()));
                }
                start = page.last_evaluated_key().cloned();
                if start.is_none() {
                    break;
                }
                assert!(page_number < 19, "parallel Scan did not finish");
            }
        }
        assert_eq!(
            scanned,
            batch_ids
                .iter()
                .cloned()
                .chain(std::iter::once("process".into()))
                .collect()
        );
    }
    let expires = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_sub(1);
    sdk.put_item()
        .table_name("ProcessData")
        .item("id", AttributeValue::S("expired".into()))
        .item("expires", AttributeValue::N(expires.to_string()))
        .send()
        .await
        .unwrap();
    sdk.update_time_to_live()
        .table_name("ProcessData")
        .time_to_live_specification(
            TimeToLiveSpecification::builder()
                .attribute_name("expires")
                .enabled(true)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let ttl = sdk
        .describe_time_to_live()
        .table_name("ProcessData")
        .send()
        .await
        .unwrap();
    assert_eq!(
        ttl.time_to_live_description()
            .and_then(|description| description.attribute_name()),
        Some("expires")
    );
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let item = sdk
            .get_item()
            .table_name("ProcessData")
            .key("id", AttributeValue::S("expired".into()))
            .send()
            .await
            .unwrap();
        if item.item().is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "TTL worker did not delete expired item"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let updated = HashMap::from([
        ("id".into(), AttributeValue::S("process".into())),
        ("value".into(), AttributeValue::S("updated".into())),
    ]);
    let update = |id: &str, value: &str| {
        TransactWriteItem::builder()
            .update(
                Update::builder()
                    .table_name("ProcessData")
                    .key("id", AttributeValue::S(id.into()))
                    .update_expression("SET #v = :v")
                    .expression_attribute_names("#v", "value")
                    .expression_attribute_values(":v", AttributeValue::S(value.into()))
                    .build()
                    .unwrap(),
            )
            .build()
    };
    let check = |condition: &str| {
        TransactWriteItem::builder()
            .condition_check(
                ConditionCheck::builder()
                    .table_name("ProcessData")
                    .key("id", AttributeValue::S(condition_key.clone()))
                    .condition_expression(condition)
                    .build()
                    .unwrap(),
            )
            .build()
    };
    sdk.transact_write_items()
        .client_request_token("process-update-check")
        .transact_items(update("process", "updated"))
        .transact_items(update(&second_write_key, "updated"))
        .transact_items(check("attribute_not_exists(id)"))
        .send()
        .await
        .unwrap();
    sdk.transact_write_items()
        .client_request_token("process-update-check")
        .transact_items(update("process", "updated"))
        .transact_items(update(&second_write_key, "updated"))
        .transact_items(check("attribute_not_exists(id)"))
        .send()
        .await
        .unwrap();
    assert!(
        sdk.transact_write_items()
            .client_request_token("process-update-check")
            .transact_items(update("process", "different"))
            .transact_items(update(&second_write_key, "different"))
            .transact_items(check("attribute_not_exists(id)"))
            .send()
            .await
            .is_err()
    );
    assert!(
        sdk.transact_write_items()
            .transact_items(update("process", "rolled back"))
            .transact_items(update(&second_write_key, "rolled back"))
            .transact_items(check("attribute_exists(id)"))
            .send()
            .await
            .is_err()
    );
    let transaction_reads = ["process", second_write_key.as_str(), condition_key.as_str()]
        .into_iter()
        .map(|id| {
            aws_sdk_dynamodb::types::TransactGetItem::builder()
                .get(
                    aws_sdk_dynamodb::types::Get::builder()
                        .table_name("ProcessData")
                        .key("id", AttributeValue::S(id.into()))
                        .projection_expression("#v")
                        .expression_attribute_names("#v", "value")
                        .build()
                        .unwrap(),
                )
                .build()
        })
        .collect::<Vec<_>>();
    let snapshot = sdk
        .transact_get_items()
        .set_transact_items(Some(transaction_reads.clone()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        snapshot.responses()[0].item().unwrap().get("value"),
        Some(&AttributeValue::S("updated".into()))
    );
    assert_eq!(
        snapshot.responses()[1].item().unwrap().get("value"),
        Some(&AttributeValue::S("updated".into()))
    );
    assert!(snapshot.responses()[2].item().is_none());
    let mut large_keys = Vec::new();
    for side in [range("process"), range(&second_write_key)] {
        large_keys.extend(
            (0..1_000)
                .map(|i| format!("large-{i}"))
                .filter(|id| range(id) == side && range(&format!("escaped-{id}")) == side)
                .take(5)
                .map(|id| ("ProcessData".into(), id)),
        );
    }
    let large = support::LargeTransaction::write(&sdk, large_keys).await;
    let read_keys = (0..1_000)
        .map(|i| format!("large-read-{i}"))
        .filter(|id| range(id) == range("process"))
        .take(14)
        .collect();
    let large_read = support::LargeTransaction::single_cell(&sdk, "ProcessData", read_keys).await;
    // Historical coordinator count exceeds the binary's 64 active-Cell slots.
    // Every request still follows the signed SDK path and touches two data Cells.
    let mut shards = std::collections::HashSet::new();
    let resident_tokens = (0..10_000)
        .map(|n| format!("resident-{n}"))
        .filter(|token| {
            shards.insert(
                beyonddb::coordinator_target("123456789012", token.as_bytes())
                    .unwrap()
                    .cell_id(),
            )
        })
        .take(70)
        .collect::<Vec<_>>();
    assert_eq!(resident_tokens.len(), 70);
    for token in &resident_tokens {
        sdk.transact_write_items()
            .client_request_token(token)
            .transact_items(update("process", "updated"))
            .transact_items(update(&second_write_key, "updated"))
            .send()
            .await
            .unwrap();
    }
    child.kill().unwrap();
    child.wait().unwrap();
    // A replacement process can receive a new private address. Durable Cell
    // ownership must follow its fenced session, including historical coordinators.
    let replacement_peer = loop {
        let address = free_addr();
        if address != peer {
            break address;
        }
    };
    let mut replacement_config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    replacement_config["peer_bind"] = json!(replacement_peer);
    replacement_config["peer_endpoint"] = json!(format!("https://{replacement_peer}"));
    fs::write(&config, replacement_config.to_string()).unwrap();
    let mut restarted = start(&config, &log, false, s3);
    wait_healthy(&mut restarted, public, &log);
    large.assert_recovered(&sdk).await;
    large_read.assert_recovered(&sdk).await;
    sdk.transact_write_items()
        .client_request_token("process-update-check")
        .transact_items(update("process", "updated"))
        .transact_items(update(&second_write_key, "updated"))
        .transact_items(check("attribute_not_exists(id)"))
        .send()
        .await
        .unwrap();
    sdk.transact_write_items()
        .client_request_token(&resident_tokens[0])
        .transact_items(update("process", "updated"))
        .transact_items(update(&second_write_key, "updated"))
        .send()
        .await
        .unwrap();
    let read = sdk
        .get_item()
        .table_name("ProcessData")
        .key("id", AttributeValue::S("process".into()))
        .send()
        .await
        .unwrap();
    assert_eq!(read.item(), Some(&updated));
    let recovered_snapshot = sdk
        .transact_get_items()
        .set_transact_items(Some(transaction_reads))
        .send()
        .await
        .unwrap();
    assert_eq!(recovered_snapshot.responses(), snapshot.responses());
    let second = sdk
        .get_item()
        .table_name("ProcessData")
        .key("id", AttributeValue::S(second_write_key.clone()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        second.item().unwrap().get("value"),
        Some(&AttributeValue::S("updated".into()))
    );
    let recovered_batch = read_batch().send().await.unwrap();
    let recovered = &recovered_batch.responses().unwrap()["ProcessData"];
    assert!(batch_items.iter().all(|item| recovered.contains(item)));
    let recovered_ttl = sdk
        .describe_time_to_live()
        .table_name("ProcessData")
        .send()
        .await
        .unwrap();
    assert_eq!(
        recovered_ttl
            .time_to_live_description()
            .and_then(|description| description.attribute_name()),
        Some("expires")
    );
    let recovered_tags = sdk
        .list_tags_of_resource()
        .resource_arn(arn)
        .send()
        .await
        .unwrap();
    assert_eq!(recovered_tags.tags().len(), 2);
    assert_eq!(recovered_tags.tags()[0].key(), "created");
    assert_eq!(recovered_tags.tags()[0].value(), "yes");
    assert_eq!(recovered_tags.tags()[1].key(), "team");
    assert_eq!(recovered_tags.tags()[1].value(), "elastic");
    sdk.update_time_to_live()
        .table_name("ProcessData")
        .time_to_live_specification(
            TimeToLiveSpecification::builder()
                .attribute_name("expires")
                .enabled(false)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    stop(&mut restarted, &log);
    let mut drained = start(&config, &log, false, s3);
    wait_healthy(&mut drained, public, &log);
    let disabled_ttl = sdk
        .describe_time_to_live()
        .table_name("ProcessData")
        .send()
        .await
        .unwrap();
    assert!(
        disabled_ttl
            .time_to_live_description()
            .and_then(|description| description.attribute_name())
            .is_none()
    );
    // Graceful drain leaves ranges Idle. Discovery must reopen their published
    // roots as well as taking over expired active owners after a hard restart.
    let idle_read = sdk
        .get_item()
        .table_name("ProcessData")
        .key("id", AttributeValue::S("process".into()))
        .consistent_read(true)
        .send()
        .await
        .unwrap();
    assert_eq!(idle_read.item(), Some(&updated));
    sdk.delete_table()
        .table_name("ProcessData")
        .send()
        .await
        .unwrap();
    sdk.create_table()
        .table_name("ProcessData")
        .key_schema(
            aws_sdk_dynamodb::types::KeySchemaElement::builder()
                .attribute_name("id")
                .key_type(aws_sdk_dynamodb::types::KeyType::Hash)
                .build()
                .unwrap(),
        )
        .attribute_definitions(
            aws_sdk_dynamodb::types::AttributeDefinition::builder()
                .attribute_name("id")
                .attribute_type(aws_sdk_dynamodb::types::ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .billing_mode(aws_sdk_dynamodb::types::BillingMode::PayPerRequest)
        .send()
        .await
        .unwrap_or_else(|error| {
            panic!(
                "table recreation failed: {error:?}; status {:?}; log {}",
                drained.try_wait(),
                fs::read_to_string(&log).unwrap()
            )
        });
    let recreated_tags = sdk
        .list_tags_of_resource()
        .resource_arn(arn)
        .send()
        .await
        .unwrap();
    assert!(recreated_tags.tags().is_empty());
    let recreated_ttl = sdk
        .describe_time_to_live()
        .table_name("ProcessData")
        .send()
        .await
        .unwrap();
    assert!(
        recreated_ttl
            .time_to_live_description()
            .and_then(|description| description.attribute_name())
            .is_none()
    );
    sdk.describe_table()
        .table_name("ProcessData")
        .send()
        .await
        .unwrap();
    stop(&mut drained, &log);
    rustfs.kill().unwrap();
    rustfs.wait().unwrap();
}
