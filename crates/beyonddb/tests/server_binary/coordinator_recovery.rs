use crate::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires local rustfs and aws CLI"]
async fn settled_history_beyond_residency_survives_process_loss() {
    let started = Instant::now();
    let mut fixture = process_fixture(2).await;
    let sdk = &fixture.sdk;
    let created = sdk
        .create_table()
        .table_name("CoordinatorRecovery")
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
    let table_id = created.table_description().unwrap().table_id().unwrap();
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
            >> 7
    };
    let other = (0..1_000)
        .map(|n| format!("other-{n}"))
        .find(|key| range(key) != range("first"))
        .unwrap();
    let mut shards = HashSet::new();
    let tokens = (0..10_000)
        .map(|n| format!("recovery-{n}"))
        .filter(|token| {
            shards.insert(
                beyonddb::coordinator_target("123456789012", token.as_bytes())
                    .unwrap()
                    .cell_id(),
            )
        })
        .take(70)
        .collect::<Vec<_>>();
    assert_eq!(tokens.len(), 70);
    for (version, token) in tokens.iter().enumerate() {
        let items = ["first", other.as_str()].map(|key| {
            TransactWriteItem::builder()
                .update(
                    Update::builder()
                        .table_name("CoordinatorRecovery")
                        .key("id", AttributeValue::S(key.into()))
                        .update_expression("SET #v = :version")
                        .expression_attribute_names("#v", "version")
                        .expression_attribute_values(
                            ":version",
                            AttributeValue::N(version.to_string()),
                        )
                        .build()
                        .unwrap(),
                )
                .build()
        });
        sdk.transact_write_items()
            .client_request_token(token)
            .set_transact_items(Some(items.to_vec()))
            .send()
            .await
            .expect(token);
    }
    eprintln!("70 settled shards committed in {:?}", started.elapsed());
    fixture.child.kill().unwrap();
    fixture.child.wait().unwrap();
    let replacement_peer = loop {
        let address = free_addr();
        if address != fixture.peer {
            break address;
        }
    };
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(&fixture.config).unwrap()).unwrap();
    config["peer_bind"] = json!(replacement_peer);
    config["peer_endpoint"] = json!(format!("https://{replacement_peer}"));
    fs::write(&fixture.config, config.to_string()).unwrap();
    let recovery_started = Instant::now();
    fixture.child = start(&fixture.config, &fixture.log, false, fixture.s3);
    let readiness = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_healthy(&mut fixture.child, fixture.public, &fixture.log);
    }));
    if let Err(failure) = readiness {
        // Preserve the failed storage image for startup-only replay. Rebuilding
        // its history would hide the failing phase behind another long SDK run.
        eprintln!("failed recovery fixture: {}", fixture.root.keep().display());
        std::panic::resume_unwind(failure);
    }
    eprintln!("replacement healthy in {:?}", recovery_started.elapsed());
    for key in ["first", other.as_str()] {
        let recovered = sdk
            .get_item()
            .table_name("CoordinatorRecovery")
            .key("id", AttributeValue::S(key.into()))
            .consistent_read(true)
            .send()
            .await
            .unwrap();
        assert_eq!(
            recovered.item().unwrap().get("version"),
            Some(&AttributeValue::N("69".into()))
        );
    }
    stop(&mut fixture.child, &fixture.log);
}
