use super::*;

fn scan(run: Value) -> Upload {
    serde_json::from_value(json!({
        "request_id":Uuid::now_v7(),"head_sha":"a".repeat(40),"category":"audit",
        "sarif":{"version":"2.1.0","runs":[run]}
    }))
    .unwrap()
}

#[test]
fn driver_severity_and_indexed_artifacts_determine_the_annotation() {
    let report = scan(json!({
        "tool":{"driver":{"name":"Audit","rules":[{
            "id":"test-01","defaultConfiguration":{"level":"error"}
        }]}},
        "originalUriBaseIds":{"%SRCROOT%":{"uri":"file:///runner/project"}},
        "artifacts":[{"location":{"uri":"file:///runner/project/source%20file.txt"}}],
        "results":[{
            "rule":{"id":"test-01","index":0},"message":{"text":"Check the input."},
            "locations":[{"physicalLocation":{
                "artifactLocation":{"index":0},"region":{"startLine":2,"endLine":5}
            }}]
        }, {
            "ruleId":"no-longer-present","level":"error","baselineState":"absent",
            "message":{"text":"Absent finding."}
        }]
    }))
    .into_check()
    .unwrap();
    assert_eq!(report.conclusion, Some(CheckConclusion::Failure));
    assert_eq!(
        report.output.annotations,
        [CheckAnnotation {
            path: "source file.txt".into(),
            start_line: 2,
            end_line: 5,
            level: AnnotationLevel::Failure,
            title: Some("test-01".into()),
            message: "Check the input.".into(),
        }]
    );
    assert!(
        report
            .output
            .summary
            .starts_with("Errors 1 · Warnings 0 · Notes 0")
    );
}

#[test]
fn out_of_checkout_paths_keep_the_finding_without_exposing_the_runner_path() {
    let mut upload = scan(json!({
        "tool":{"driver":{"name":"Audit"}},
        "results":[{"level":"warning","message":{"text":"Review the configuration."},
            "locations":[{"physicalLocation":{
                "artifactLocation":{"uri":"file:///runner/config"},
                "region":{"startLine":1}
        }}]}]
    }));
    upload.checkout_uri = Some(Url::parse("file:///runner/project").unwrap());
    upload.fail_on = FailOn::Warning;
    let report = upload.into_check().unwrap();
    assert_eq!(report.conclusion, Some(CheckConclusion::Failure));
    assert!(report.output.annotations.is_empty());
    let text = report.output.text.unwrap();
    assert!(text.contains("Review the configuration."));
    assert!(!text.contains("/runner"));
}

#[test]
fn missing_and_oversize_results_cannot_pass_a_check() {
    let tool = json!({"driver":{"name":"Audit"}});
    assert!(scan(json!({"tool":tool})).into_check().is_err());
    assert!(
        scan(json!({
            "tool":tool,
            "results":vec![json!({"level":"note","message":{"text":"Read the docs."}}); 51],
        }))
        .into_check()
        .is_err()
    );
    let failed = scan(json!({
        "tool":tool,"invocations":[{"executionSuccessful":false}]
    }))
    .into_check()
    .unwrap();
    assert_eq!(failed.conclusion, Some(CheckConclusion::Failure));
}
