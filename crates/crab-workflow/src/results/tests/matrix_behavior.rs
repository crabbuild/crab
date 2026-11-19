use super::chart_behavior::retained_plot;
use super::*;

#[test]
fn large_matrices_count_every_observation_before_display_bounding() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let runs = [
        [
            (0, 0),
            (0, 0),
            (0, 0),
            (0, 1),
            (1, 0),
            (1, 0),
            (1, 1),
            (1, 1),
        ],
        [
            (0, 0),
            (0, 1),
            (0, 1),
            (0, 1),
            (1, 0),
            (1, 0),
            (1, 0),
            (1, 1),
        ],
    ]
    .into_iter()
    .enumerate()
    .map(|(index, pattern)| {
        let mut bytes = String::from("actual,predicted,alternative\n");
        for row in 0..100_000 {
            let (actual, predicted) = pattern[row % pattern.len()];
            bytes.push_str(&format!("{actual},{predicted},{}\n", 1 - predicted));
        }
        let mut snapshot =
            retained_plot(&local, "classes.csv", bytes.as_bytes(), Some("confusion")).snapshot;
        snapshot.name = Some(format!("matrix-run-{index}"));
        snapshot.plots = ["confusion", "confusion_normalized"]
            .into_iter()
            .map(|template| {
                serde_json::from_value(json!({"id":template,"path":"classes.csv","x":"actual",
                "y":["predicted","alternative"],"template":template}))
                .unwrap()
            })
            .collect();
        snapshot.seal().unwrap()
    })
    .collect::<Vec<_>>();
    let charts = charts(&runs, |hash| local.blob(hash)).unwrap();
    for chart in &charts {
        assert_eq!(chart.state, Availability::Present, "{:?}", chart.reason);
        assert_eq!(chart.points, 400_000);
        assert_eq!(chart.displayed_points, 16);
        assert_eq!(serde_json::to_value(chart).unwrap()["reduction"], "counted");
        let spec = chart.spec.as_ref().unwrap();
        let weight = spec["transform"][0]["aggregate"][0]["field"]
            .as_str()
            .unwrap();
        let mut counts = spec["data"]["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row[weight].as_u64().unwrap())
            .collect::<Vec<_>>();
        counts.sort();
        let expected = [vec![12_500; 6], vec![25_000; 4], vec![37_500; 6]].concat();
        assert_eq!(counts, expected);
    }
    let report = Report {
        schema_version: RESULTS_SCHEMA_VERSION,
        renderer: RESULTS_RENDERER.into(),
        charts,
        comparison: compare(&runs, &spec(&runs[0])).unwrap(),
        title: "Synthetic exact large matrices".into(),
        created_at: "2026-11-18T00:00:00Z".into(),
    };
    if let Some(output) = std::env::var_os("CRAB_RESULTS_EVIDENCE") {
        let html = report_html(&report, |hash| local.blob(hash)).unwrap();
        std::fs::write(
            std::path::PathBuf::from(output).join("chart-large-matrices.html"),
            html,
        )
        .unwrap();
    }
}

#[test]
fn matrix_class_identities_distinguish_missing_literal_and_delimited_values() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let bytes = br#"[{"actual":null,"predicted":"null"},{"actual":"null","predicted":null},
        {"actual":"a|b","predicted":"c"},{"actual":"a","predicted":"b|c"}]"#;
    let mut snapshot = retained_plot(&local, "classes.json", bytes, Some("confusion")).snapshot;
    snapshot.plots[0].x = Some("actual".into());
    snapshot.plots[0].y = vec!["predicted".into()];
    let captured = snapshot.seal().unwrap();
    let chart = charts(&[captured.clone()], |hash| local.blob(hash))
        .unwrap()
        .remove(0);
    assert_eq!(
        serde_json::to_value(&chart).unwrap()["reduction"],
        "counted"
    );
    assert_eq!(chart.points, 4);
    assert_eq!(chart.displayed_points, 4);
    let report = Report {
        schema_version: RESULTS_SCHEMA_VERSION,
        renderer: RESULTS_RENDERER.into(),
        charts: vec![chart],
        comparison: compare(&[captured.clone()], &spec(&captured)).unwrap(),
        title: "Synthetic exact class identities".into(),
        created_at: "2026-11-18T00:00:00Z".into(),
    };
    if let Some(output) = std::env::var_os("CRAB_RESULTS_EVIDENCE") {
        let html = report_html(&report, |hash| local.blob(hash)).unwrap();
        std::fs::write(
            std::path::PathBuf::from(output).join("chart-matrix-identities.html"),
            html,
        )
        .unwrap();
    }
}

#[test]
fn named_matrix_templates_remain_authored_when_they_override_a_builtin() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(".dvc/plots");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(
        path.join("confusion.json"),
        br#"{"data":{"values":"<DVC_METRIC_DATA>"},
        "mark":"point","encoding":{"x":{"field":"<DVC_METRIC_X>","type":"quantitative"},
        "y":{"field":"<DVC_METRIC_Y>","type":"quantitative"}}}"#,
    )
    .unwrap();
    let config: crate::PlotConfig = serde_json::from_value(
        json!({"path":"curve.csv","x":"step","y":["loss"],"template":"confusion"}),
    )
    .unwrap();
    let data =
        plots::parse_plot_data_bytes(&config.path, b"step,loss\n0,1\n1,1\n", &config).unwrap();
    let spec =
        plot_specs::build_vega_spec(&config, "Authored override", &data, directory.path()).unwrap();
    assert_eq!(spec["mark"], "point");
    assert_eq!(spec["data"]["values"][1]["loss"], 1);
    assert!(spec.get("transform").is_none());
}

#[test]
fn matrix_numeric_classes_keep_javascript_equality_and_numeric_order() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let bytes = b"step,loss\n10,0\n2,0\n1e0,0\n1.0,0\n-0,0\n-0.0,0\n0.0,0\n";
    let captured = retained_plot(&local, "classes.csv", bytes, Some("confusion"));
    let chart = charts(&[captured], |hash| local.blob(hash))
        .unwrap()
        .remove(0);
    assert_eq!(chart.displayed_points, 4);
    let spec = chart.spec.unwrap();
    assert_eq!(spec["encoding"]["x"]["sort"], json!([3, 2, 1, 0]));
    let weight = spec["transform"][0]["aggregate"][0]["field"]
        .as_str()
        .unwrap();
    let mut counts = spec["data"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row[weight].as_u64().unwrap())
        .collect::<Vec<_>>();
    counts.sort();
    assert_eq!(counts, vec![1, 1, 2, 3]);
}

#[test]
fn matrix_labels_are_data_and_dimension_roles_stay_distinct() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let bytes = b"count,normalized\ndatum.value,datum.label\ndatum.label,datum.value\n";
    let mut snapshot = retained_plot(&local, "labels.csv", bytes, Some("confusion")).snapshot;
    snapshot.name = Some("datum.value datum.label".into());
    snapshot.plots[0].x = Some("count".into());
    snapshot.plots[0].y = vec!["normalized".into()];
    let captured = snapshot.seal().unwrap();
    let chart = charts(&[captured.clone()], |hash| local.blob(hash))
        .unwrap()
        .remove(0);
    let spec = chart.spec.as_ref().unwrap();
    let titles = spec["encoding"]["tooltip"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| {
            field["title"]
                .as_str()
                .unwrap_or(field["field"].as_str().unwrap())
                .to_owned()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(titles.len(), 4);
    let labels = spec["params"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|param| param["value"].as_array().unwrap().iter())
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        labels.contains(&"\"datum.value\"")
            && labels.contains(&"\"datum.label\"")
            && labels
                .iter()
                .any(|label| label.starts_with("datum.value datum.label"))
    );
    let report = Report {
        schema_version: RESULTS_SCHEMA_VERSION,
        renderer: RESULTS_RENDERER.into(),
        charts: vec![chart],
        comparison: compare(&[captured.clone()], &super::spec(&captured)).unwrap(),
        title: "Synthetic literal matrix labels".into(),
        created_at: "2026-11-18T00:00:00Z".into(),
    };
    if let Some(output) = std::env::var_os("CRAB_RESULTS_EVIDENCE") {
        std::fs::write(
            std::path::PathBuf::from(output).join("chart-matrix-labels.html"),
            report_html(&report, |hash| local.blob(hash)).unwrap(),
        )
        .unwrap();
    }
}
