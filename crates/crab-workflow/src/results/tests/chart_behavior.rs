use super::*;
use std::path::Path;

pub(super) fn retained_plot(
    local: &LocalResults,
    path: &str,
    bytes: &[u8],
    template: Option<&str>,
) -> SealedResult {
    let mut snapshot = run(json!({"score": 1})).snapshot;
    snapshot.plots = vec![
        serde_json::from_value(
            json!({"path": path, "x": "step", "y": ["loss"], "template": template}),
        )
        .unwrap(),
    ];
    snapshot.attachments.push(Attachment {
        path: path.into(),
        kind: AttachmentKind::Plot,
        stage: None,
        hash: Some(local.put_blob(bytes).unwrap()),
        size: bytes.len() as u64,
        state: Availability::Present,
        reason: None,
        push: true,
        remote: None,
    });
    snapshot.seal().unwrap()
}

#[tokio::test]
async fn comparison_keeps_healthy_curves_when_retained_plot_bytes_are_unavailable() {
    use object_store::ObjectStoreExt;

    for (remote_read, cross_file) in [(false, false), (true, false), (false, true), (true, true)] {
        for missing in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let local = LocalResults::new(directory.path()).unwrap();
            let runs = [
                retained_plot(&local, "curve.csv", b"step,loss\n0,1\n1,2\n", None),
                retained_plot(&local, "curve.csv", b"step,loss\n0,3\n1,4\n", None),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, run)| {
                let mut snapshot = run.snapshot;
                if cross_file {
                    snapshot.plots[0].x_path = Some("steps.csv".into());
                    let bytes = if index == 0 {
                        b"step,unused\n10,0\n20,0\n"
                    } else {
                        b"step,unused\n30,0\n40,0\n"
                    };
                    let coordinates = retained_plot(&local, "steps.csv", bytes, None);
                    snapshot
                        .attachments
                        .extend(coordinates.snapshot.attachments);
                }
                snapshot.seal().unwrap()
            })
            .collect::<Vec<_>>();
            let store = Arc::new(object_store::memory::InMemory::new());
            let remote = RemoteResults::new(
                WorkflowStore::new(store.clone()),
                "repository",
                CancellationToken::new(),
            );
            for run in &runs {
                local.install(run).unwrap();
                remote.push(&local, run).await.unwrap();
            }
            let path = if cross_file { "steps.csv" } else { "curve.csv" };
            let hash = runs[0]
                .snapshot
                .attachments
                .iter()
                .find(|attachment| attachment.path == path)
                .unwrap()
                .hash
                .as_ref()
                .unwrap();
            let object = directory
                .path()
                .join(".crab/workflow/results/objects")
                .join(hash);
            let key = object_store::path::Path::from(format!(
                "repository/workflow/results/objects/{hash}"
            ));
            if missing {
                std::fs::remove_file(object).unwrap();
                store.delete(&key).await.unwrap();
            } else {
                std::fs::write(object, b"corrupt retained plot").unwrap();
                store
                    .put(
                        &key,
                        bytes::Bytes::from_static(b"corrupt retained plot").into(),
                    )
                    .await
                    .unwrap();
            }
            let reader = if remote_read {
                ResultsReader::Remote(Box::new(remote))
            } else {
                ResultsReader::Local(local)
            };
            let response = reader
                .read(ResultsRead::Compare {
                    runs: runs
                        .iter()
                        .map(|run| RunSelection {
                            id: run.snapshot.experiment_id.clone(),
                            digest: run.digest.clone(),
                        })
                        .collect(),
                    spec: spec(&runs[0]),
                })
                .await
                .unwrap();
            let ResultsResponse::Comparison(view) = response else {
                panic!("expected comparison")
            };
            let chart = &view.charts[0];
            assert_eq!(
                (
                    view.comparison.rows.len(),
                    chart.state,
                    chart.points,
                    chart.spec.is_some(),
                    chart.interaction.as_ref().and_then(|axes| axes.domain),
                ),
                (
                    2,
                    if missing {
                        Availability::Missing
                    } else {
                        Availability::Corrupt
                    },
                    2,
                    true,
                    Some(AxisRange {
                        lower: if cross_file { 30.0 } else { 0.0 },
                        upper: if cross_file { 40.0 } else { 1.0 },
                    }),
                ),
                "remote={remote_read}, missing={missing}, cross_file={cross_file}",
            );
        }
    }
}

#[test]
fn unavailable_charts_keep_classified_states_without_provider_diagnostics() {
    use std::io::ErrorKind;

    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    for input in ["curve.csv", "steps.csv", "plots/authored.json"] {
        let captured = retained_plot(&local, "curve.csv", b"step,loss\n0,1\n", None);
        let mut unavailable_hash = captured.snapshot.attachments[0].hash.clone().unwrap();
        let mut snapshot = captured.snapshot;
        if input != "curve.csv" {
            let bytes = if input == "steps.csv" {
                b"step,loss\n10,1\n".as_slice()
            } else {
                br#"{"data":{"values":"<DVC_METRIC_DATA>"},"mark":"line"}"#.as_slice()
            };
            let mut attachment = retained_plot(&local, input, bytes, None)
                .snapshot
                .attachments
                .remove(0);
            unavailable_hash = attachment.hash.clone().unwrap();
            if input == "steps.csv" {
                snapshot.plots[0].x_path = Some(input.into());
            } else {
                snapshot.plots[0].template = Some(input.into());
                attachment.kind = AttachmentKind::Template;
            }
            snapshot.attachments.push(attachment);
        }
        let captured = snapshot.seal().unwrap();
        for (kind, state) in [
            (ErrorKind::NotFound, Availability::Missing),
            (ErrorKind::PermissionDenied, Availability::Denied),
            (ErrorKind::ConnectionRefused, Availability::Unsupported),
        ] {
            let chart = charts(std::slice::from_ref(&captured), |hash| {
                if hash != unavailable_hash {
                    return local.blob(hash).map_err(Arc::new);
                }
                Err(Arc::new(crate::WorkflowError::Io(std::io::Error::new(
                    kind,
                    "private provider diagnostic",
                ))))
            })
            .unwrap()
            .remove(0);
            assert_eq!(
                (chart.state, chart.spec.is_none(), chart.points),
                (state, true, 0)
            );
            assert!(
                !serde_json::to_string(&chart)
                    .unwrap()
                    .contains("private provider diagnostic")
            );
        }
    }
}

#[test]
fn missing_plot_values_are_gaps_without_changing_numeric_axes_or_cli_rows() {
    for (path, bytes, expected_rows) in [
        (
            "curve.csv",
            b"step,loss\n0,1\n1,\n2,3\n".as_slice(),
            json!(["1", "", "3"]),
        ),
        (
            "curve.tsv",
            b"step\tloss\n0\t1\n1\t\n2\t3\n".as_slice(),
            json!(["1", "", "3"]),
        ),
        (
            "curve.json",
            br#"[{"step":0,"loss":1},{"step":1,"loss":null},{"step":2,"loss":3}]"#.as_slice(),
            json!(["1", "null", "3"]),
        ),
        (
            "curve.yaml",
            b"- {step: 0, loss: 1}\n- {step: 1, loss: null}\n- {step: 2, loss: 3}\n".as_slice(),
            json!(["1", "null", "3"]),
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let local = LocalResults::new(directory.path()).unwrap();
        let captured = retained_plot(&local, path, bytes, None);
        let config = &captured.snapshot.plots[0];
        let parsed = plots::parse_plot_data_bytes(Path::new(path), bytes, config).unwrap();
        let cli_rows = serde_json::to_value(&parsed).unwrap()["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["y_values"][0].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            json!(cli_rows),
            expected_rows,
            "{path}: tagged CLI string rows"
        );
        let chart = charts(&[captured], |hash| local.blob(hash))
            .unwrap()
            .remove(0);
        let spec = chart.spec.unwrap();
        assert_eq!(spec["encoding"]["y"]["type"], "quantitative", "{path}");
        assert!(
            spec["data"]["values"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["step"] == 1 && row["value"].is_null()),
            "{path}"
        );
    }
}

#[test]
fn curve_sampling_retains_missing_intervals_and_endpoints() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let mut bytes = String::from("epoch,loss\n");
    for step in 0..10_000 {
        if (1234..1238).contains(&step) {
            bytes.push_str(&format!("{},\n", step * 2));
        } else {
            bytes.push_str(&format!("{},{step}\n", step * 2));
        }
    }
    let mut captured = retained_plot(&local, "curve.csv", bytes.as_bytes(), None).snapshot;
    captured.plots[0].x = Some("epoch".into());
    let captured = captured.seal().unwrap();
    let chart = charts(&[captured], |hash| local.blob(hash))
        .unwrap()
        .remove(0);
    let spec = chart.spec.unwrap();
    let values = spec["data"]["values"].as_array().unwrap();
    assert!(
        values
            .iter()
            .any(|row| row["step"] == 1234 && row["value"].is_null())
    );
    assert_eq!(
        (
            values.first().unwrap()["step"].clone(),
            values.last().unwrap()["step"].clone()
        ),
        (json!(0), json!(9999))
    );
    assert!(values.len() <= MAX_SERIES_POINTS);
}

#[test]
fn sampling_refuses_more_missing_transitions_than_the_display_budget() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let mut bytes = String::from("step,loss\n");
    for step in 0..10_000 {
        let value = if step % 2 == 0 {
            String::new()
        } else {
            step.to_string()
        };
        bytes.push_str(&format!("{step},{value}\n"));
    }
    let captured = retained_plot(&local, "curve.csv", bytes.as_bytes(), None);
    let chart = charts(&[captured], |hash| local.blob(hash))
        .unwrap()
        .remove(0);
    assert_eq!(chart.state, Availability::Unsupported);
    assert!(chart.reason.unwrap().contains("missing intervals"));
}

#[test]
fn equal_template_paths_require_equal_captured_bytes_for_overlays() {
    for differing in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let local = LocalResults::new(directory.path()).unwrap();
        let runs = (0..2).map(|index| {
            let mut snapshot = retained_plot(&local, "curve.csv", b"step,loss\n0,1\n1,2\n", Some("plots/authored.json")).snapshot;
            let template = serde_json::to_vec(&json!({
                "$schema":"https://vega.github.io/schema/vega-lite/v5.json",
                "title": if differing && index == 1 { "Changed meaning" } else { "Original meaning" },
                "data":{"values":"<DVC_METRIC_DATA>"}, "mark":"line",
                "encoding":{"x":{"field":"<DVC_METRIC_X>","type":"quantitative"},
                    "y":{"field":"<DVC_METRIC_Y>","type":"quantitative"},
                    "color":{"field":"<DVC_METRIC_COLOR>","type":"nominal"}}
            })).unwrap();
            snapshot.attachments.push(Attachment {
                path:"plots/authored.json".into(),kind:AttachmentKind::Template,stage:None,
                hash:Some(local.put_blob(&template).unwrap()),size:template.len() as u64,
                state:Availability::Present,reason:None,push:true,remote:None,
            });
            snapshot.seal().unwrap()
        }).collect::<Vec<_>>();
        let chart = charts(&runs, |hash| local.blob(hash)).unwrap().remove(0);
        assert_eq!(
            chart.state,
            if differing {
                Availability::Unsupported
            } else {
                Availability::Present
            }
        );
        if differing {
            assert!(chart.reason.unwrap().contains("template bytes differ"));
        }
    }
}

#[test]
fn confusion_grid_expansion_is_bounded_before_rendering() {
    for (classes, run_count, supported) in
        [(44, 1, true), (45, 1, false), (31, 2, true), (32, 2, false)]
    {
        let directory = tempfile::tempdir().unwrap();
        let local = LocalResults::new(directory.path()).unwrap();
        let mut bytes = String::from("step,loss\n");
        for class in 0..classes {
            bytes.push_str(&format!("{class},{class}\n"));
        }
        let runs = (0..run_count)
            .map(|_| {
                retained_plot(
                    &local,
                    "matrix.csv",
                    bytes.as_bytes(),
                    Some("confusion_normalized"),
                )
            })
            .collect::<Vec<_>>();
        let chart = charts(&runs, |hash| local.blob(hash)).unwrap().remove(0);
        assert_eq!(
            chart.state,
            if supported {
                Availability::Present
            } else {
                Availability::Unsupported
            }
        );
        if !supported {
            assert!(chart.reason.unwrap().contains("display cells"));
        }
    }
}

#[test]
fn oversized_transforms_refuse_sampling_that_would_change_their_statistics() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let mut bytes = String::from("step,loss\n");
    for step in 0..=MAX_SERIES_POINTS {
        bytes.push_str(&format!("{},{}\n", step % 2, step % 3));
    }
    for template in ["smooth", "bar_horizontal"] {
        let captured = retained_plot(&local, "curve.csv", bytes.as_bytes(), Some(template));
        let chart = charts(&[captured], |hash| local.blob(hash))
            .unwrap()
            .remove(0);
        assert_eq!(chart.state, Availability::Unsupported, "{template}");
        assert!(chart.reason.unwrap().contains("sampling"));
    }
}

#[test]
fn cross_file_coordinates_keep_absence_separate_from_literal_categories() {
    let config: crate::PlotConfig =
        serde_json::from_value(json!({"path":"curve.json", "x":"epoch", "y":["loss"]})).unwrap();
    let mut y = plots::parse_plot_data_bytes(
        Path::new("curve.json"),
        br#"[{"loss":"null"},{"loss":"-"},{"loss":3}]"#,
        &config,
    )
    .unwrap();
    let x = plots::parse_plot_data_bytes(
        Path::new("x.json"),
        br#"[{"epoch":0},{"epoch":null},{"epoch":2}]"#,
        &config,
    )
    .unwrap();
    for (row, source) in y.rows.iter_mut().zip(x.rows) {
        row.replace_x(source);
    }
    let spec = plot_specs::build_vega_spec(&config, "Categories", &y, Path::new(".")).unwrap();
    assert_eq!(spec["data"]["values"][0]["value"], "null");
    assert_eq!(spec["data"]["values"][1]["value"], "-");
    assert!(spec["data"]["values"][1]["epoch"].is_null());
    assert_eq!(
        (
            spec["encoding"]["x"]["type"].clone(),
            spec["encoding"]["y"]["type"].clone()
        ),
        (json!("quantitative"), json!("nominal"))
    );
}

#[test]
fn plots_refuse_excess_series_and_normalized_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let oversized = "x".repeat(1024 * 1024 + 1);
    let bytes = format!("step,loss\n0,{oversized}\n1,{oversized}\n");
    let captured = retained_plot(&local, "curve.csv", bytes.as_bytes(), None);
    assert!(
        charts(&[captured], |hash| local.blob(hash))
            .unwrap_err()
            .to_string()
            .contains("2 MiB")
    );
    let mut captured = retained_plot(&local, "curve.csv", b"step,loss\n0,1\n", None).snapshot;
    captured.plots[0].y = (0..=MAX_COMPARISON_RUNS)
        .map(|index| format!("loss{index}"))
        .collect();
    let chart = charts(&[captured.seal().unwrap()], |hash| local.blob(hash))
        .unwrap()
        .remove(0);
    assert_eq!(chart.state, Availability::Unsupported);
    assert!(chart.reason.unwrap().contains("20 compared series"));
}

#[test]
#[ignore = "dedicated maximum-overlay renderer qualification"]
fn maximum_overlays_emit_bounded_renderer_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let runs = (0..MAX_COMPARISON_RUNS)
        .map(|offset| {
            let mut bytes = String::from("step,loss\n");
            for step in 0..MAX_SERIES_POINTS {
                bytes.push_str(&format!("{step},{}\n", step % 17 + offset));
            }
            retained_plot(&local, "curve.csv", bytes.as_bytes(), Some("linear"))
        })
        .collect::<Vec<_>>();
    let started = std::time::Instant::now();
    let charts = charts(&runs, |hash| local.blob(hash)).unwrap();
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    let bytes = encode(&charts).unwrap().len();
    assert_eq!(charts[0].points, 40_000);
    assert!(charts[0].displayed_points <= 2000);
    assert_eq!(
        charts[0].spec.as_ref().unwrap()["data"]["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["rev"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        MAX_COMPARISON_RUNS
    );
    assert!(bytes <= 2 * 1024 * 1024);
    let comparison = compare(&runs, &spec(&runs[0])).unwrap();
    let report = Report {
        schema_version: RESULTS_SCHEMA_VERSION,
        renderer: RESULTS_RENDERER.into(),
        charts,
        title: "Synthetic maximum-overlay qualification".into(),
        comparison,
        created_at: "2026-11-18T00:00:00Z".into(),
    };
    let html = report_html(&report, |hash| local.blob(hash)).unwrap();
    let mut semantic = retained_plot(
        &local,
        "gaps.csv",
        b"step,loss\n0,1\n1,2\n2,\n3,4\n4,5\n",
        None,
    )
    .snapshot;
    let complete = b"step,loss\n0,1\n1,2\n2,3\n3,4\n4,5\n";
    semantic.attachments.push(Attachment {
        path: "complete.csv".into(),
        kind: AttachmentKind::Plot,
        stage: None,
        hash: Some(local.put_blob(complete).unwrap()),
        size: complete.len() as u64,
        state: Availability::Present,
        reason: None,
        push: true,
        remote: None,
    });
    let encoding = json!({
        "x":{"field":"<DVC_METRIC_X>","type":"quantitative"},
        "y":{"field":"<DVC_METRIC_Y>","type":"quantitative"},
        "color":{"field":"<DVC_METRIC_COLOR>","type":"nominal"}
    });
    for (id, template) in [
        (
            "scaled",
            json!({
                "$schema":"https://vega.github.io/schema/vega-lite/v5.json",
                "title":"Scaled retained values", "data":{"values":"<DVC_METRIC_DATA>"},
                "transform":[{"calculate":"datum.loss * 10", "as":"scaled_loss"}],
                "mark":"line", "encoding":{
                    "x":encoding["x"], "y":{"field":"scaled_loss", "type":"quantitative"}, "color":encoding["color"]
                }
            }),
        ),
        (
            "compound",
            json!({
                "$schema":"https://vega.github.io/schema/vega-lite/v5.json",
                "title":"Authored compound view", "data":{"values":"<DVC_METRIC_DATA>"},
                "hconcat":[
                    {"mark":"line", "width":240, "encoding":encoding},
                    {"mark":"bar", "width":240, "encoding":{
                        "x":{"field":"<DVC_METRIC_COLOR>", "type":"nominal"},
                        "y":{"field":"<DVC_METRIC_Y>", "type":"quantitative", "aggregate":"mean"}
                    }}
                ]
            }),
        ),
    ] {
        let path = format!("plots/{id}.json");
        let bytes = serde_json::to_vec(&template).unwrap();
        semantic.attachments.push(Attachment {
            path: path.clone(),
            kind: AttachmentKind::Template,
            stage: None,
            hash: Some(local.put_blob(&bytes).unwrap()),
            size: bytes.len() as u64,
            state: Availability::Present,
            reason: None,
            push: true,
            remote: None,
        });
        semantic.plots.push(
            serde_json::from_value(json!({
                "id":id, "path":"complete.csv", "x":"step", "y":["loss"], "template":path
            }))
            .unwrap(),
        );
    }
    let semantic = semantic.seal().unwrap();
    let semantic_report = Report {
        charts: super::charts(&[semantic.clone()], |hash| local.blob(hash)).unwrap(),
        comparison: compare(&[semantic.clone()], &spec(&semantic)).unwrap(),
        title: "Synthetic retained plot semantics".into(),
        ..report.clone()
    };
    assert!(
        semantic_report
            .charts
            .iter()
            .filter(|chart| chart.key != "gaps.csv")
            .all(|chart| chart.state == Availability::Present && chart.interaction.is_none())
    );
    let semantic_html = report_html(&semantic_report, |hash| local.blob(hash)).unwrap();
    let evidence = json!({"runs":runs.len(), "points_per_series":MAX_SERIES_POINTS,
        "displayed_points":report.charts[0].displayed_points,
        "normalized_bytes":bytes, "chart_build_ms": elapsed,
        "platform":std::env::consts::OS, "architecture":std::env::consts::ARCH});
    if let Some(output) = std::env::var_os("CRAB_RESULTS_EVIDENCE") {
        let output = std::path::PathBuf::from(output);
        std::fs::write(
            output.join("chart-scale.json"),
            serde_json::to_vec_pretty(&evidence).unwrap(),
        )
        .unwrap();
        std::fs::write(output.join("chart-scale.html"), html).unwrap();
        std::fs::write(output.join("chart-semantics.html"), semantic_html).unwrap();
    }
    println!("{evidence}");
}

#[test]
fn builtin_comparisons_emit_revision_isolation_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let runs = (0..2)
        .map(|offset| {
            let mut bytes = String::from("step,loss\n");
            for step in 0..10 {
                bytes.push_str(&format!("{step},{}\n", step + offset * 100));
            }
            let mut snapshot =
                retained_plot(&local, "curve.csv", bytes.as_bytes(), Some("smooth")).snapshot;
            let matrix = if offset == 0 {
                b"actual,predicted\n0,0\n0,0\n0,1\n1,1\n1,1\n1,1\n".as_slice()
            } else {
                b"actual,predicted\n0,0\n1,0\n1,0\n1,0\n1,1\n1,1\n".as_slice()
            };
            snapshot.attachments.push(Attachment {
                path: "matrix.csv".into(),
                kind: AttachmentKind::Plot,
                stage: None,
                hash: Some(local.put_blob(matrix).unwrap()),
                size: matrix.len() as u64,
                state: Availability::Present,
                reason: None,
                push: true,
                remote: None,
            });
            for (id, template) in [
                ("confusion", "confusion"),
                ("normalized", "confusion_normalized"),
            ] {
                snapshot.plots.push(
                    serde_json::from_value(json!({"id":id, "path":"matrix.csv",
                "x":"actual", "y":["predicted"], "template":template}))
                    .unwrap(),
                );
            }
            snapshot.plots.push(
                serde_json::from_value(json!({"id":"minimal", "path":"curve.csv",
            "x":"step", "y":["loss"], "template":"simple"}))
                .unwrap(),
            );
            snapshot.seal().unwrap()
        })
        .collect::<Vec<_>>();
    let charts = charts(&runs, |hash| local.blob(hash)).unwrap();
    assert!(
        charts
            .iter()
            .all(|chart| chart.state == Availability::Present)
    );
    let report = Report {
        schema_version: RESULTS_SCHEMA_VERSION,
        renderer: RESULTS_RENDERER.into(),
        charts,
        comparison: compare(&runs, &spec(&runs[0])).unwrap(),
        title: "Synthetic independent run statistics".into(),
        created_at: "2026-11-18T00:00:00Z".into(),
    };
    let html = report_html(&report, |hash| local.blob(hash)).unwrap();
    if let Some(output) = std::env::var_os("CRAB_RESULTS_EVIDENCE") {
        std::fs::write(
            std::path::PathBuf::from(output).join("chart-builtins.html"),
            html,
        )
        .unwrap();
    }
}
