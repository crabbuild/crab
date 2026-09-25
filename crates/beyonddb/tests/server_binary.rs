#![cfg(unix)]

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    ops::{Deref, DerefMut},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use aws_credential_types::Credentials;
use aws_sdk_dynamodb::types::AttributeValue;
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
    let deadline = Instant::now() + Duration::from_secs(20);
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
async fn bootstrap_sdk_write_survives_server_process_restart() {
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
    while TcpStream::connect_timeout(&s3, Duration::from_millis(200)).is_err() {
        assert!(Instant::now() < deadline, "RustFS did not start");
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
    stop(&mut child, &log);
    let mut restarted = start(&config, &log, false, s3);
    wait_healthy(&mut restarted, public, &log);
    let read = sdk
        .get_item()
        .table_name("ProcessData")
        .key("id", AttributeValue::S("process".into()))
        .send()
        .await
        .unwrap();
    assert_eq!(read.item(), Some(&item));
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
        .unwrap();
    let recreated_tags = sdk
        .list_tags_of_resource()
        .resource_arn(arn)
        .send()
        .await
        .unwrap();
    assert!(recreated_tags.tags().is_empty());
    stop(&mut restarted, &log);
    rustfs.kill().unwrap();
    rustfs.wait().unwrap();
}
