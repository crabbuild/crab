use super::*;
use std::collections::BTreeSet;
use std::path::Path;

fn capture(local: &LocalResults, offset: u32, template: &str) -> SealedResult {
    let bytes = format!(
        "step,train_loss,val_loss\n0,{},{}\n1,{},{}\n",
        1 + offset,
        2 + offset,
        3 + offset,
        4 + offset
    );
    let mut snapshot = run(json!({"score":offset})).snapshot;
    snapshot.plots = vec![
        serde_json::from_value(json!({
            "path":"curve.csv", "x":"step", "y":["train_loss","val_loss"], "template":template
        }))
        .unwrap(),
    ];
    snapshot.attachments.push(Attachment {
        path: "curve.csv".into(),
        kind: AttachmentKind::Plot,
        stage: None,
        hash: Some(local.put_blob(bytes.as_bytes()).unwrap()),
        size: bytes.len() as u64,
        state: Availability::Present,
        reason: None,
        push: true,
        remote: None,
    });
    snapshot.seal().unwrap()
}

#[test]
fn templates_keep_each_selected_y_field_separate_in_every_run() {
    for template in ["linear", "simple", "scatter", "smooth"] {
        let directory = tempfile::tempdir().unwrap();
        let local = LocalResults::new(directory.path()).unwrap();
        let runs = [capture(&local, 0, template), capture(&local, 100, template)];
        let chart = charts(&runs, |hash| local.blob(hash)).unwrap().remove(0);
        assert_eq!(
            chart.state,
            Availability::Present,
            "{template}: {:?}",
            chart.reason
        );
        let spec = chart.spec.unwrap();
        let y = spec["encoding"]["y"]["field"].as_str().unwrap();
        let color = spec["encoding"]["color"]["field"].as_str().unwrap();
        let values = spec["data"]["values"].as_array().unwrap();
        let mut series = BTreeMap::<String, Vec<u64>>::new();
        for row in values {
            series
                .entry(row[color].as_str().unwrap().into())
                .or_default()
                .push(row[y].as_u64().unwrap());
        }
        let mut actual = series.into_values().collect::<Vec<_>>();
        actual.sort();
        assert_eq!(
            actual,
            vec![vec![1, 3], vec![2, 4], vec![101, 103], vec![102, 104]],
            "{template}"
        );
    }
}

#[test]
fn authored_interactions_keep_complete_hover_data() {
    let mut evidence = Vec::new();
    for name in ["tooltip", "configured", "selected"] {
        let mut spec = json!({
            "$schema":"https://vega.github.io/schema/vega-lite/v5.json",
            "width":280,"height":160,
            "data":{"values":[
                {"step":0,"score":91,"rev":"Run A"},
                {"step":1,"score":93,"rev":"Run A"},
                {"step":0,"score":17,"rev":"Run B"},
                {"step":1,"score":18,"rev":"Run B"}
            ]},
            "mark":{"type":"line","point":true,"tooltip":true},
            "encoding":{
                "x":{"field":"step","type":"quantitative"},
                "y":{"field":"score","type":"quantitative"},
                "color":{"field":"rev","type":"nominal"}
            }
        });
        if name == "tooltip" {
            spec["encoding"]["tooltip"] = json!([
                {"field":"score","type":"quantitative"},
                {"field":"rev","type":"nominal"}
            ]);
        } else if name == "configured" {
            spec["config"] = json!({"mark":{"tooltip":{"content":"data"}}});
        } else {
            spec["params"] = json!([{"name":"selected","select":{
                "type":"point","fields":["rev"],"on":"pointerover","clear":"pointerout"
            }}]);
            spec["encoding"]["opacity"] =
                json!({"condition":{"param":"selected","value":1},"value":0.2});
        }
        let bounds = BTreeMap::from([(
            "step".into(),
            AxisRange {
                lower: 0.0,
                upper: 1.0,
            },
        )]);
        let interaction =
            super::super::interactions::augment(&mut spec, &bounds, &BTreeSet::new()).unwrap();
        assert!(name == "selected" || interaction.is_some());
        evidence.push(json!({"name":name,"spec":spec,"interaction":interaction}));
    }
    if let Some(output) = std::env::var_os("CRAB_RESULTS_EVIDENCE") {
        std::fs::write(
            std::path::PathBuf::from(output).join("chart-authored-hover.json"),
            serde_json::to_vec(&evidence).unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn dvc_optional_anchors_preserve_json_types_and_raw_columns() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let template = json!({
        "$schema":"https://vega.github.io/schema/vega-lite/v5.json",
        "data":{"values":"<DVC_METRIC_DATA>"}, "mark":"line",
        "width":"<DVC_METRIC_PLOT_WIDTH>", "height":"<DVC_METRIC_PLOT_HEIGHT>",
        "params":["<DVC_METRIC_ZOOM_AND_PAN>"],
        "transform":[{"calculate":"datum.train_loss - datum.val_loss", "as":"difference"}],
        "encoding":{
            "x":{"field":"<DVC_METRIC_X>","type":"quantitative"},
            "y":{"field":"<DVC_METRIC_Y>","type":"quantitative"},
            "color":"<DVC_METRIC_COLOR>", "strokeDash":"<DVC_METRIC_STROKE_DASH>",
            "shape":"<DVC_METRIC_SHAPE>", "tooltip":"<DVC_METRIC_TOOLTIP>"
        },
        "usermeta":{
            "group":"<DVC_METRIC_GROUP_BY>","group_x":"<DVC_METRIC_GROUP_BY_X>",
            "group_y":"<DVC_METRIC_GROUP_BY_Y>","pivot":"<DVC_METRIC_PIVOT_FIELD>",
            "row":"<DVC_METRIC_ROW>","column":"<DVC_METRIC_COLUMN>"
        }
    });
    let bytes = serde_json::to_vec(&template).unwrap();
    let runs = [0, 100].map(|offset| {
        let mut snapshot = capture(&local, offset, "plots/current.json").snapshot;
        snapshot.attachments.push(Attachment {
            path: "plots/current.json".into(),
            kind: AttachmentKind::Template,
            stage: None,
            hash: Some(local.put_blob(&bytes).unwrap()),
            size: bytes.len() as u64,
            state: Availability::Present,
            reason: None,
            push: true,
            remote: None,
        });
        snapshot.seal().unwrap()
    });
    let chart = charts(&runs, |hash| local.blob(hash)).unwrap().remove(0);
    assert_eq!(chart.state, Availability::Present, "{:?}", chart.reason);
    let spec = chart.spec.unwrap();
    assert!(spec["width"].is_number() && spec["height"].is_number());
    assert_eq!(spec["params"][0]["bind"], "scales");
    assert!(spec["encoding"]["color"].is_object());
    assert!(spec["encoding"]["tooltip"].is_array());
    assert_eq!(spec["usermeta"]["group"], json!(["rev", "field"]));
    assert_eq!(spec["usermeta"]["group_x"], json!(["rev", "field", "step"]));
    assert!(spec["usermeta"]["group_y"].as_array().unwrap().len() == 3);
    assert!(
        spec["usermeta"]["pivot"]
            .as_str()
            .unwrap()
            .contains("datum")
    );
    for anchor in ["strokeDash", "shape"] {
        assert_eq!(spec["encoding"][anchor]["field"], "field");
    }
    for row in spec["data"]["values"].as_array().unwrap() {
        assert_eq!(
            row["val_loss"].as_i64().unwrap() - row["train_loss"].as_i64().unwrap(),
            1
        );
    }
    let runs = runs.map(|run| {
        let mut snapshot = run.snapshot;
        for template in ["linear", "smooth"] {
            let mut plot = snapshot.plots[0].clone();
            plot.id = Some(template.into());
            plot.template = Some(template.into());
            snapshot.plots.push(plot);
        }
        for (id, path, bytes, x, y, template) in [
            (
                "literal",
                "literal.csv",
                b"epoch.step[0],loss.value\\raw\n0,1\n1,2\n".as_slice(),
                "epoch.step[0]",
                "loss.value\\raw",
                Some("simple"),
            ),
            (
                "bars",
                "bars.csv",
                b"score,label\n1,cat\n2,dog\n".as_slice(),
                "score",
                "label",
                Some("bar_horizontal_sorted"),
            ),
            (
                "literal-default",
                "literal.csv",
                b"epoch.step[0],loss.value\\raw\n0,1\n1,2\n".as_slice(),
                "epoch.step[0]",
                "loss.value\\raw",
                None,
            ),
        ] {
            if !snapshot
                .attachments
                .iter()
                .any(|attachment| attachment.path == path)
            {
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
            }
            snapshot.plots.push(
                serde_json::from_value(
                    json!({"id":id,"path":path,"x":x,"y":[y],"template":template}),
                )
                .unwrap(),
            );
        }
        snapshot.seal().unwrap()
    });
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
        comparison: compare(&runs, &super::spec(&runs[0])).unwrap(),
        title: "Synthetic DVC template parity".into(),
        created_at: "2026-11-18T00:00:00Z".into(),
    };
    if let Some(output) = std::env::var_os("CRAB_RESULTS_EVIDENCE") {
        let html = report_html(&report, |hash| local.blob(hash)).unwrap();
        std::fs::write(
            std::path::PathBuf::from(output).join("chart-template-parity.html"),
            html,
        )
        .unwrap();
    }
}

#[test]
fn shared_dvc_plot_ids_retain_all_source_files() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let mut snapshot = capture(&local, 0, "linear").snapshot;
    snapshot.plots[0].id = Some("loss".into());
    let mut second = snapshot.plots[0].clone();
    second.path = "validation.csv".into();
    snapshot.plots.push(second);
    let mut attachment = snapshot.attachments[0].clone();
    attachment.path = "validation.csv".into();
    snapshot.attachments.push(attachment);
    let captured = snapshot.seal().unwrap();
    let charts = charts(&[captured], |hash| local.blob(hash)).unwrap();
    assert_eq!(charts.len(), 2);
    assert_ne!(charts[0].key, charts[1].key);
    assert!(
        charts
            .iter()
            .all(|chart| chart.state == Availability::Present)
    );
}

#[test]
fn literal_field_names_are_escaped_for_vega_without_renaming_data() {
    let directory = tempfile::tempdir().unwrap();
    let config: crate::PlotConfig = serde_json::from_value(json!({
        "path":"curve.csv", "x":"epoch.step[0]", "y":["loss.value\\raw"], "template":"simple"
    }))
    .unwrap();
    let data = plots::parse_plot_data_bytes(
        &config.path,
        b"epoch.step[0],loss.value\\raw\n0,1\n1,2\n",
        &config,
    )
    .unwrap();
    let spec =
        plot_specs::build_vega_spec(&config, "Literal columns", &data, directory.path()).unwrap();
    assert_eq!(spec["encoding"]["x"]["field"], "epoch\\.step\\[0\\]");
    let y = spec["encoding"]["y"]["field"].as_str().unwrap();
    assert_eq!(spec["data"]["values"][1][y], 2);
    assert_eq!(spec["data"]["values"][1]["loss.value\\raw"], 2);
    assert_eq!(spec["data"]["values"][1]["epoch.step[0]"], 1);
}

#[test]
fn template_metadata_never_overwrites_selected_source_columns() {
    let directory = tempfile::tempdir().unwrap();
    let config: crate::PlotConfig = serde_json::from_value(json!({
        "path":"reserved.csv", "x":"step", "y":["rev","series","__crab_value"], "template":"simple"
    }))
    .unwrap();
    let data = plots::parse_plot_data_bytes(
        &config.path,
        b"step,rev,series,__crab_value\n0,1,2,3\n",
        &config,
    )
    .unwrap();
    let spec = plot_specs::build_vega_spec(&config, "Metadata collisions", &data, directory.path())
        .unwrap();
    let y = spec["encoding"]["y"]["field"].as_str().unwrap();
    let rows = spec["data"]["values"].as_array().unwrap();
    assert_eq!(
        rows.iter().map(|row| row[y].clone()).collect::<Vec<_>>(),
        vec![json!(1), json!(2), json!(3)]
    );
    assert!(
        rows.iter()
            .all(|row| row["rev"] == 1 && row["series"] == 2 && row["__crab_value"] == 3)
    );
}

#[test]
fn compound_templates_keep_typed_auxiliary_fields_without_duplicating_observations() {
    let directory = tempfile::tempdir().unwrap();
    let template = json!({
        "data":{"values":"<DVC_METRIC_DATA>"},
        "facet":{"column":{"field":"cohort","type":"nominal"}},
        "spec":{"layer":[
            {"mark":"area","encoding":{"x":{"field":"epoch","type":"quantitative"},
                "y":{"field":"lower","type":"quantitative"},"y2":{"field":"upper"}}},
            {"mark":"line","encoding":{"y":{"field":"<DVC_METRIC_Y>","type":"quantitative"},
                "color":"<DVC_METRIC_COLOR>"}}
        ]}
    });
    std::fs::write(directory.path().join("compound.json"), template.to_string()).unwrap();
    for (path, bytes) in [
        ("scores.json", br#"[{"epoch":1,"score":0.8,"lower":0.7,"upper":0.9,"cohort":"mobile","enabled":true,"note":null,"rev":"authored","step":42}]"#.as_slice()),
        ("scores.yaml", b"- epoch: 1\n  score: 0.8\n  lower: 0.7\n  upper: 0.9\n  cohort: mobile\n  enabled: true\n  note: null\n  rev: authored\n  step: 42\n".as_slice()),
        ("scores.csv", b"epoch,score,lower,upper,cohort,enabled,note,rev,step\n1,0.8,0.7,0.9,mobile,true,,authored,42\n".as_slice()),
        ("scores.tsv", b"epoch\tscore\tlower\tupper\tcohort\tenabled\tnote\trev\tstep\n1\t0.8\t0.7\t0.9\tmobile\ttrue\t\tauthored\t42\n".as_slice()),
    ] {
        let config: crate::PlotConfig = serde_json::from_value(json!({
            "path":path,"x":"epoch","y":["score"],"template":"compound.json"
        })).unwrap();
        let data = plots::parse_plot_data_bytes(&config.path, bytes, &config).unwrap();
        let spec = plot_specs::build_vega_spec(&config, "Cohorts", &data, directory.path()).unwrap();
        let rows = spec["data"]["values"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{path}: one row per selected measure");
        let row = &rows[0];
        assert_eq!((row["lower"].clone(), row["upper"].clone(), row["cohort"].clone()),
            (json!(0.7), json!(0.9), json!("mobile")), "{path}");
        assert!(row["note"].is_null(), "{path}");
        assert_eq!((row["rev"].clone(), row["step"].clone()), (json!("authored"), json!(42)));
        let color = spec["spec"]["layer"][1]["encoding"]["color"]["field"].as_str().unwrap();
        assert_ne!(color, "rev");
        assert_eq!(row["enabled"], if path.ends_with("json") || path.ends_with("yaml") {
            json!(true)
        } else { json!("true") });
        assert_eq!(serde_json::to_value(&data).unwrap(), json!({
            "y_fields":["score"],"rows":[{"x":"1","y_values":["0.8"]}]
        }), "tagged CLI rows remain unchanged");
    }
}

#[test]
fn cross_file_template_coordinates_override_only_x_source_fields() {
    let directory = tempfile::tempdir().unwrap();
    let config: crate::PlotConfig = serde_json::from_value(json!({
        "path":"scores.json","x":"epoch\\axis","y":["score"],"template":"simple"
    }))
    .unwrap();
    let mut data = plots::parse_plot_data_bytes(
        &config.path,
        br#"[{"epoch\\axis":99,"score":0.8,"cohort":"mobile","__crab_x":42}]"#,
        &config,
    )
    .unwrap();
    let source = plots::parse_plot_data_bytes(
        Path::new("x.json"),
        br#"[{"epoch\\axis":3,"cohort":"must not replace"}]"#,
        &config,
    )
    .unwrap();
    data.rows[0].replace_x(source.rows.into_iter().next().unwrap());
    let spec = plot_specs::build_vega_spec(&config, "Cross-file", &data, directory.path()).unwrap();
    assert_eq!(spec["data"]["values"][0]["epoch\\axis"], 3);
    assert_eq!(spec["data"]["values"][0]["cohort"], "mobile");
    assert_eq!(spec["data"]["values"][0]["__crab_x"], 42);
    assert_eq!(spec["encoding"]["x"]["field"], "__crab_x_");
}

#[test]
fn horizontal_bar_templates_keep_dvc_numeric_x_and_categorical_y() {
    let directory = tempfile::tempdir().unwrap();
    for template in ["bar_horizontal", "bar_horizontal_sorted"] {
        let config: crate::PlotConfig = serde_json::from_value(json!({
            "path":"bars.csv", "x":"score", "y":["label"], "template":template
        }))
        .unwrap();
        let data =
            plots::parse_plot_data_bytes(&config.path, b"score,label\n1,cat\n2,dog\n", &config)
                .unwrap();
        let spec = plot_specs::build_vega_spec(&config, "Scores", &data, directory.path()).unwrap();
        assert_eq!(
            (
                spec["encoding"]["x"]["field"].as_str(),
                spec["encoding"]["y"]["field"].as_str()
            ),
            (Some("score"), Some("label"))
        );
    }
}

#[test]
fn multi_y_confusion_bounds_include_every_selected_class_column() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let mut bytes = String::from("step,train_loss,val_loss\n");
    for class in 0..100 {
        bytes.push_str(&format!("{class},0,{class}\n"));
    }
    let mut snapshot = capture(&local, 0, "confusion").snapshot;
    snapshot.attachments[0].hash = Some(local.put_blob(bytes.as_bytes()).unwrap());
    snapshot.attachments[0].size = bytes.len() as u64;
    let chart = charts(&[snapshot.seal().unwrap()], |hash| local.blob(hash))
        .unwrap()
        .remove(0);
    assert!(
        chart.spec.is_none()
            && chart
                .reason
                .as_deref()
                .unwrap()
                .contains("2,000 display cells")
    );
}

#[test]
fn dvc_anchor_validation_does_not_reinterpret_captured_values() {
    let directory = tempfile::tempdir().unwrap();
    let config: crate::PlotConfig = serde_json::from_value(json!({
        "path":"labels.csv", "x":"step", "y":["label"], "template":"custom.json"
    }))
    .unwrap();
    let data = plots::parse_plot_data_bytes(
        &config.path,
        b"step,label\n0,<DVC_METRIC_UNKNOWN>\n",
        &config,
    )
    .unwrap();
    let path = directory.path().join("custom.json");
    std::fs::write(
        &path,
        br#"{"data":{"values":"<DVC_METRIC_DATA>"},"title":"<DVC_METRIC_TITLE>"}"#,
    )
    .unwrap();
    let spec =
        plot_specs::build_vega_spec(&config, "<DVC_METRIC_UNKNOWN>", &data, directory.path())
            .unwrap();
    assert_eq!(spec["data"]["values"][0]["label"], "<DVC_METRIC_UNKNOWN>");
    std::fs::write(path, br#"{"title":"<DVC_METRIC_UNKNOWN>"}"#).unwrap();
    assert!(plot_specs::build_vega_spec(&config, "Title", &data, directory.path()).is_err());
}

#[test]
fn literal_backslash_coordinates_keep_linked_numeric_domains() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let mut snapshot = capture(&local, 0, "linear").snapshot;
    snapshot.plots[0].x = Some("epoch\\axis".into());
    let bytes = b"epoch\\axis,train_loss,val_loss\n0,1,2\n1,3,4\n";
    snapshot.attachments[0].hash = Some(local.put_blob(bytes).unwrap());
    snapshot.attachments[0].size = bytes.len() as u64;
    let chart = charts(&[snapshot.seal().unwrap()], |hash| local.blob(hash))
        .unwrap()
        .remove(0);
    let axis = chart.interaction.unwrap();
    assert_eq!(
        axis.domain,
        Some(AxisRange {
            lower: 0.0,
            upper: 1.0
        })
    );
    let spec = chart.spec.unwrap();
    assert_eq!(spec["data"]["values"][2][&axis.field], 1);
    assert_eq!(spec["data"]["values"][2]["epoch\\axis"], 1);
}
