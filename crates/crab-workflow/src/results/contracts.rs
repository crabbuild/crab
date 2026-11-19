use super::*;
use std::path::PathBuf;
use ts_rs::{Config, TS};

fn declarations() -> String {
    let c = Config::default().with_large_int("number");
    let types = [
        serde_json::Value::decl(&c),
        Availability::decl(&c),
        Datum::decl(&c),
        AttachmentKind::decl(&c),
        Attachment::decl(&c),
        InputWitness::decl(&c),
        OutputWitness::decl(&c),
        StageResult::decl(&c),
        RuntimeWitness::decl(&c),
        ReproductionSpec::decl(&c),
        crate::PlotConfig::decl(&c),
        ResultOrigin::decl(&c),
        ResultSnapshot::decl(&c),
        SealedResult::decl(&c),
        Direction::decl(&c),
        DatasetIdentity::decl(&c),
        AxisRange::decl(&c),
        ChartInteraction::decl(&c),
        ComparisonSpec::decl(&c),
        ComparisonCell::decl(&c),
        ComparisonRow::decl(&c),
        Comparison::decl(&c),
        Report::decl(&c),
        RunQuery::decl(&c),
        RunSummary::decl(&c),
        Column::decl(&c),
        RunPage::decl(&c),
        Chart::decl(&c),
        ChartReduction::decl(&c),
        ChartImage::decl(&c),
        RunSelection::decl(&c),
        ResultsRead::decl(&c),
        ComparisonView::decl(&c),
        ReportView::decl(&c),
        EvidenceView::decl(&c),
        ResultsResponse::decl(&c),
        ReportDraft::decl(&c),
        ArtifactPromotionRequest::decl(&c),
        ActionPreview::decl(&c),
    ];
    let mut source =
        String::from("// Generated from crab-workflow Results contracts; do not edit.\n");
    for declaration in types {
        source.push_str("\n// prettier-ignore\nexport ");
        source.push_str(&declaration);
        source.push('\n');
    }
    source
}
fn paths() -> [PathBuf; 3] {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    [
        root.join("packages/workflow-results/contracts.ts"),
        root.join("packages/desktop/src/lib/results-contracts.ts"),
        root.join("packages/ui/src/lib/results-contracts.ts"),
    ]
}
fn client_export() -> &'static str {
    "// Generated from crab-workflow Results contracts; do not edit.\nexport type * from \"../../../workflow-results/contracts\";\n"
}
#[test]
fn client_contracts_match_owner() {
    for (index, path) in paths().into_iter().enumerate() {
        let expected = if index == 0 {
            declarations()
        } else {
            client_export().into()
        };
        assert_eq!(
            std::fs::read_to_string(path).unwrap().replace("\r\n", "\n"),
            expected
        );
    }
}
#[test]
#[ignore = "explicitly regenerates Results client declarations"]
fn export_results_contracts() {
    for (index, path) in paths().into_iter().enumerate() {
        let source = if index == 0 {
            declarations()
        } else {
            client_export().into()
        };
        std::fs::write(path, source).unwrap();
    }
}
