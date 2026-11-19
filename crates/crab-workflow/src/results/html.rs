use base64::Engine as _;
use serde_json::json;
use std::collections::BTreeMap;

use super::*;
use crate::Result;

pub const CHART_LICENSES: &str = include_str!("../../assets/RESULTS-LICENSES.txt");
pub const CHART_RUNTIME: &str = include_str!("../../assets/results-runtime.js");

/// Exports a frozen comparison and retained charts as a self-contained offline document.
pub fn report_html(report: &Report, read: impl Fn(&str) -> Result<Vec<u8>>) -> Result<String> {
    report.verify()?;
    let charts = &report.charts;
    let mut objects = BTreeMap::new();
    let mut total = 0;
    // Verify and embed retained evidence; exported reports have no storage dependency.
    for attachment in
        super::local::report_evidence(report).filter(|a| a.state == Availability::Present)
    {
        let hash = attachment
            .hash
            .as_deref()
            .ok_or_else(|| super::invalid("report attachment lacks identity"))?;
        if objects.contains_key(hash) {
            continue;
        }
        total += attachment.size;
        if total > MAX_CAPTURE_BYTES as u64 {
            return Err(super::invalid(
                "offline evidence exceeds 256 MiB; select fewer runs",
            ));
        }
        let bytes = read(hash)?;
        if bytes.len() as u64 != attachment.size || super::digest(&bytes) != hash {
            return Err(super::stored(
                hash,
                super::invalid("export evidence identity mismatch"),
            ));
        }
        objects.insert(
            hash.to_owned(),
            base64::engine::general_purpose::STANDARD.encode(bytes),
        );
    }
    let payload = serde_json::to_string(
        &json!({"id": report.id()?, "report": report, "charts": charts, "objects": objects}),
    )
    .map_err(crate::WorkflowError::ResultsJson)?
    .replace('<', "\\u003c")
    .replace('>', "\\u003e")
    .replace('&', "\\u0026");
    let title = report
        .title
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    Ok(format!(
        r#"<!doctype html><html lang="en"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; base-uri 'none'; form-action 'none'">
<title>{title}</title><style>body{{font:14px system-ui;margin:32px;color:#171717;background:white}}main{{max-width:1200px;margin:auto}}table{{border-collapse:collapse;width:100%}}th,td{{text-align:left;padding:10px;border-bottom:1px solid #ddd}}code,#identity,pre{{overflow-wrap:anywhere}}section{{margin:24px 0;overflow:auto}}.muted{{color:#666}}pre{{white-space:pre-wrap}}button{{padding:8px}}</style>
<main><h1>{title}</h1><p id="identity" class="muted"></p><section id="comparison"></section><div id="charts"></div><section><h2>Retained evidence</h2><div id="attachments"></div><div id="preview"></div></section><details><summary>Frozen evidence manifests</summary><pre id="evidence"></pre></details><details><summary>Open source notices</summary><pre>{licenses}</pre></details></main>
<script id="results-data" type="application/json">{payload}</script><script>{runtime}</script>
<script>
{report_ui}
</script></html>"#,
        runtime = CHART_RUNTIME,
        report_ui = include_str!("../../assets/results-report.js"),
        licenses = CHART_LICENSES
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    ))
}
