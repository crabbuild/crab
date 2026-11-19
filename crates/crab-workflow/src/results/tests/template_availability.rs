use super::*;
use std::io::ErrorKind;

#[test]
fn partial_template_evidence_preserves_the_verified_revision_in_either_order() {
    for (failure, captured_state, expected_state) in [
        (
            "capture_missing",
            Availability::Missing,
            Availability::Missing,
        ),
        (
            "capture_limit",
            Availability::NotCaptured,
            Availability::NotCaptured,
        ),
        (
            "missing_object",
            Availability::Present,
            Availability::Missing,
        ),
        (
            "corrupt_object",
            Availability::Present,
            Availability::Corrupt,
        ),
        ("denied_object", Availability::Present, Availability::Denied),
    ] {
        for unavailable_first in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let local = LocalResults::new(directory.path()).unwrap();
            let mut runs = Vec::new();
            let mut failed_hash = String::new();
            for index in 0..2 {
                let mut template = plot_templates::builtin_plot_template_spec("linear").unwrap();
                template["usermeta"] = json!({"revision":index});
                let bytes = serde_json::to_vec(&template).unwrap();
                let hash = local.put_blob(&bytes).unwrap();
                let state = if index == 0 {
                    Availability::Present
                } else {
                    captured_state
                };
                let path = if state == Availability::Present {
                    format!("templates/{hash}.json")
                } else {
                    format!("templates/unavailable/{}.json", digest(b"custom.json"))
                };
                let mut snapshot = chart_behavior::retained_plot(
                    &local,
                    "curve.csv",
                    if index == 0 {
                        b"step,loss\n0,1\n1,2\n"
                    } else {
                        b"step,loss\n0,3\n1,4\n"
                    },
                    Some(&path),
                )
                .snapshot;
                snapshot.attachments.push(Attachment {
                    path,
                    kind: AttachmentKind::Template,
                    stage: None,
                    hash: (state == Availability::Present).then_some(hash.clone()),
                    size: if state == Availability::Present {
                        bytes.len() as u64
                    } else {
                        0
                    },
                    state,
                    reason: None,
                    push: true,
                    remote: None,
                });
                if index == 1 {
                    failed_hash = hash;
                }
                runs.push(snapshot.seal().unwrap());
            }
            let unavailable_id = runs[1].snapshot.experiment_id.clone();
            if unavailable_first {
                runs.reverse();
            }
            let chart = charts(&runs, |hash| {
                if hash == failed_hash {
                    match failure {
                        "missing_object" => {
                            return Err(WorkflowError::Io(std::io::Error::new(
                                ErrorKind::NotFound,
                                "private provider diagnostic",
                            )));
                        }
                        "denied_object" => {
                            return Err(WorkflowError::Io(std::io::Error::new(
                                ErrorKind::PermissionDenied,
                                "private provider diagnostic",
                            )));
                        }
                        "corrupt_object" => return Ok(b"corrupt retained template".to_vec()),
                        _ => {}
                    }
                }
                local.blob(hash)
            })
            .unwrap()
            .remove(0);
            assert_eq!(
                (chart.state, chart.points, chart.spec.is_some()),
                (expected_state, 2, true),
                "failure={failure}, unavailable_first={unavailable_first}",
            );
            let values = chart.spec.as_ref().unwrap()["data"]["values"]
                .as_array()
                .unwrap();
            assert_eq!(
                values
                    .iter()
                    .map(|row| row["loss"].clone())
                    .collect::<Vec<_>>(),
                vec![json!(1), json!(2)]
            );
            let reason = chart.reason.unwrap();
            assert!(reason.contains(&unavailable_id));
            assert!(!reason.contains("private provider diagnostic"));
        }
    }
}
