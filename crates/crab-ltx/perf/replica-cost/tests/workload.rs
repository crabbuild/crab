use serde_json::Value;
use std::process::Command;

#[test]
fn writer_histories_preserve_first_mutation_and_restore_deleted_rows() {
    for mode in ["fresh", "sparse", "hydrated", "resumed"] {
        let output = Command::new(env!("CARGO_BIN_EXE_crab-ltx-replica-cost"))
            .args([
                "--activation",
                mode,
                "--churn-rows",
                "128",
                "--random-payload",
                "--payload-bytes",
                "4096",
                "--commands",
                "2",
                "--warmup",
                "1",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let activation = &report["activation"];
        let first = &activation["first_command"];
        assert_eq!(activation["mode"], mode);
        assert!(activation["initial_database_pages"].as_u64().unwrap() > 64);
        assert_eq!(first["command"], 0);
        assert_eq!(first["row"], 128);
        assert_eq!(first["mutation"], "update");
        assert_eq!(report["samples"].as_array().unwrap().len(), 1);
        assert_eq!(report["samples"][0]["command"], 1);
        assert_eq!(report["samples"][0]["mutation"], "delete");
        assert_eq!(report["restored_rows"], 127);
        let reads = first["commit_io"]
            .as_array()
            .unwrap()
            .iter()
            .chain(first["capture_io"].as_array().unwrap())
            .map(|cost| cost["bytes_read"].as_u64().unwrap())
            .sum::<u64>();
        if mode == "sparse" {
            assert!(
                reads > 0,
                "tail mutation must fault pages absent at activation"
            );
        } else {
            assert_eq!(reads, 0, "{mode}: materialized mutation read the provider");
        }
    }
}

#[test]
fn unsupported_activation_options_cannot_silently_measure_a_fresh_writer() {
    for args in [
        vec!["--sparse"],
        vec!["--activation", "unknown"],
        vec!["--activation"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_crab-ltx-replica-cost"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}
