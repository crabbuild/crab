use super::*;
#[tokio::test(flavor = "multi_thread")]
async fn typed_fields_workflow_edits_and_archival_preserve_stored_values() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let project = Uuid::now_v7();
    command(
        &cell,
        Command::Create {
            id: project,
            name: "Bug triage".into(),
            template: "bug-tracker".into(),
        },
    )
    .await;
    let mut definition = page(&cell, project).await.project.definition;
    let severity = definition.fields[0].id;
    let estimate = Uuid::now_v7();
    definition.fields.push(Field {
        id: estimate,
        name: "Estimate".into(),
        definition: FieldKind::Number,
    });
    assert!(matches!(
        command(
            &cell,
            Command::Edit {
                project,
                version: 1,
                definition: definition.clone(),
                archived: false
            }
        )
        .await,
        Outcome::Applied { version: 2, .. }
    ));
    let mut draft = Item {
        subject: None,
        title: "Broken widget".into(),
        body: String::new(),
        status: "Triage".into(),
        values: BTreeMap::from([(severity, "Critical".into()), (estimate, "NaN".into())]),
        ..entry(1)
    };
    assert!(matches!(
        command(
            &cell,
            Command::Item {
                project,
                version: 2,
                item: draft.clone()
            }
        )
        .await,
        Outcome::Rejected { .. }
    ));
    draft.values.insert(estimate, "3.5".into());
    assert!(matches!(
        command(
            &cell,
            Command::Item {
                project,
                version: 2,
                item: draft.clone()
            }
        )
        .await,
        Outcome::Applied { version: 3, .. }
    ));
    let mut invalid = definition.clone();
    invalid.statuses.retain(|s| s != "Triage");
    assert!(matches!(
        command(
            &cell,
            Command::Edit {
                project,
                version: 3,
                definition: invalid,
                archived: false
            }
        )
        .await,
        Outcome::Rejected { .. }
    ));
    let mut invalid = definition.clone();
    invalid.fields.pop();
    assert!(matches!(
        command(
            &cell,
            Command::Edit {
                project,
                version: 3,
                definition: invalid,
                archived: false
            }
        )
        .await,
        Outcome::Rejected { .. }
    ));
    if let FieldKind::Select { options } = &mut definition.fields[0].definition {
        options.push("Needs investigation".into());
    }
    assert!(matches!(
        command(
            &cell,
            Command::Edit {
                project,
                version: 3,
                definition: definition.clone(),
                archived: true
            }
        )
        .await,
        Outcome::Applied { version: 4, .. }
    ));
    draft.status = "Confirmed".into();
    assert!(matches!(
        command(
            &cell,
            Command::Item {
                project,
                version: 4,
                item: draft
            }
        )
        .await,
        Outcome::Rejected { .. }
    ));
    assert_eq!(
        page(&cell, project).await.items[0].item.values[&estimate],
        "3.5"
    );
    fixtures::close(&server).await;
}
