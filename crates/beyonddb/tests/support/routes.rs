// Fixture inspection for tables whose directory has not split into branches.
// Fleet traversal tests use bounded public pages and do not assume a global epoch.
async fn single_leaf_route(
    client: &crab_cell_runtime::client::CellClient,
    account: &crab_cell_runtime::identity::CellTarget,
    table_id: &str,
) -> Option<beyonddb::TableRoute> {
    use beyonddb::{Json, ReadRouteDirectory, RoutePageInput, RoutePageOutcome};
    let spec = client
        .query::<ReadRouteDirectory>(account, None, Json(table_id.into()))
        .await
        .unwrap()
        .output
        .0?;
    let directory = beyonddb::read_directory_leaf(client, account.tenant(), table_id, [0; 16])
        .await
        .unwrap();
    assert_eq!(
        directory.spec, spec,
        "fixture inspection requires one metadata leaf"
    );
    let table = client
        .query::<beyonddb::DescribeTableById>(account, None, Json(table_id.into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let mut route = beyonddb::TableRoute {
        table_id: table_id.into(),
        epoch: directory.version,
        partitions: vec![],
    };
    let mut input = RoutePageInput {
        table_id: table_id.into(),
        start_hash: None,
        after_lower: None,
        expected_epoch: None,
    };
    loop {
        let RoutePageOutcome::Page {
            epoch,
            partitions,
            has_more,
        } = beyonddb::read_route_page(client, account, input.clone())
            .await
            .unwrap()
        else {
            panic!("fixture route changed while inspected");
        };
        // The first page establishes this scan version. A capacity worker can
        // change membership after the separate leaf-shape inspection above.
        if input.after_lower.is_none() {
            route.epoch = epoch;
        }
        assert_eq!(epoch, route.epoch);
        input.expected_epoch = Some(epoch);
        input.after_lower = partitions.last().map(|part| part.lower);
        route
            .partitions
            .extend(partitions.into_iter().map(|part| beyonddb::PartitionSpec {
                table: table.clone(),
                partition_id: part.partition_id,
                lower: (part.lower != [0; 16]).then_some(part.lower),
                upper: part.upper,
                epoch: part.epoch,
            }));
        if !has_more {
            return Some(route);
        }
    }
}
