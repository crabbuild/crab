//! Bounded metrics and the matrix manifest attached to a receipt.

use super::*;

/// One bounded named measurement attached to a qualification receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationMetric {
    pub(in crate::qualification) name: String,
    pub(in crate::qualification) value: u64,
    pub(in crate::qualification) unit: String,
}

/// One receipt/artifact pair in a qualification matrix manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationMatrixEntry {
    pub(in crate::qualification) workload: String,
    pub(in crate::qualification) receipt: String,
    pub(in crate::qualification) artifacts: Vec<String>,
}

impl QualificationMatrixEntry {
    /// Creates one manifest entry using paths relative to the manifest file.
    pub fn new(workload: String, receipt: String, artifacts: Vec<String>) -> Result<Self> {
        validate_label(&workload, "qualification matrix workload")?;
        validate_path(&receipt, "qualification matrix receipt path")?;
        if artifacts.is_empty() || artifacts.len() > MAX_METRICS {
            return Err(Error::Control("qualification matrix artifact count"));
        }
        for artifact in &artifacts {
            validate_path(artifact, "qualification matrix artifact path")?;
        }
        Ok(Self {
            workload,
            receipt,
            artifacts,
        })
    }

    /// Returns the required workload row name.
    #[must_use]
    pub fn workload(&self) -> &str {
        &self.workload
    }

    /// Returns the receipt path relative to the manifest.
    #[must_use]
    pub fn receipt(&self) -> &str {
        &self.receipt
    }

    /// Returns raw-artifact paths relative to the manifest.
    #[must_use]
    pub fn artifacts(&self) -> &[String] {
        &self.artifacts
    }
}

/// Complete, bounded manifest for release qualification evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationMatrixManifest {
    pub(in crate::qualification) schema_version: u32,
    pub(in crate::qualification) entries: Vec<QualificationMatrixEntry>,
}

impl QualificationMatrixManifest {
    /// Builds and validates a complete matrix manifest.
    pub fn new(entries: Vec<QualificationMatrixEntry>) -> Result<Self> {
        let manifest = Self {
            schema_version: QUALIFICATION_MATRIX_SCHEMA_VERSION,
            entries,
        };
        manifest.validate_contract()?;
        Ok(manifest)
    }

    /// Decodes canonical JSON and rejects incomplete or duplicate rows.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control("qualification matrix exceeds limit"));
        }
        let manifest: Self = serde_json::from_slice(bytes)?;
        manifest.validate_contract()?;
        if serde_json::to_vec(&manifest).map_err(Error::from)? != bytes {
            return Err(Error::Control("qualification matrix is not canonical"));
        }
        Ok(manifest)
    }

    /// Encodes canonical JSON for a release artifact.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate_contract()?;
        let bytes = serde_json::to_vec(self).map_err(Error::from)?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control("qualification matrix exceeds limit"));
        }
        Ok(bytes)
    }

    /// Returns the entries in their declared manifest order.
    #[must_use]
    pub fn entries(&self) -> &[QualificationMatrixEntry] {
        &self.entries
    }

    pub(in crate::qualification) fn validate_contract(&self) -> Result<()> {
        if self.schema_version != QUALIFICATION_MATRIX_SCHEMA_VERSION
            || self.entries.len() != QUALIFICATION_MATRIX_ROWS.len()
        {
            return Err(Error::Control("qualification matrix schema or row count"));
        }
        let expected = QUALIFICATION_MATRIX_ROWS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let actual = self
            .entries
            .iter()
            .map(QualificationMatrixEntry::workload)
            .collect::<BTreeSet<_>>();
        if actual.len() != self.entries.len() || actual != expected {
            return Err(Error::Control("qualification matrix rows"));
        }
        if self
            .entries
            .iter()
            .zip(QUALIFICATION_MATRIX_ROWS.iter().copied())
            .any(|(entry, expected)| entry.workload != expected)
        {
            return Err(Error::Control("qualification matrix row order"));
        }
        for entry in &self.entries {
            QualificationMatrixEntry::new(
                entry.workload.clone(),
                entry.receipt.clone(),
                entry.artifacts.clone(),
            )?;
        }
        Ok(())
    }
}

impl QualificationMetric {
    /// Creates one measured metric, validating its name and unit labels.
    pub fn new(name: String, value: u64, unit: String) -> Result<Self> {
        validate_label(&name, "qualification metric name")?;
        validate_label(&unit, "qualification metric unit")?;
        Ok(Self { name, value, unit })
    }

    /// Returns the metric name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the measured value.
    #[must_use]
    pub const fn value(&self) -> u64 {
        self.value
    }

    /// Returns the unit the value is expressed in.
    #[must_use]
    pub fn unit(&self) -> &str {
        &self.unit
    }
}

pub(in crate::qualification) fn validate_metrics(metrics: &[QualificationMetric]) -> Result<()> {
    if metrics.len() > MAX_METRICS {
        return Err(Error::Control("qualification metric count exceeds limit"));
    }
    let mut identities = BTreeSet::new();
    for metric in metrics {
        validate_label(&metric.name, "qualification metric name")?;
        validate_label(&metric.unit, "qualification metric unit")?;
        if !identities.insert((metric.name.as_str(), metric.unit.as_str())) {
            return Err(Error::Control("duplicate qualification metric"));
        }
    }
    Ok(())
}

pub(in crate::qualification) fn validate_resource_metric_list(
    metrics: &[QualificationMetric],
) -> Result<()> {
    validate_metrics(metrics)?;
    if metrics.iter().any(|metric| {
        !QUALIFICATION_RESOURCE_METRICS
            .iter()
            .any(|(name, unit)| metric.name() == *name && metric.unit() == *unit)
    }) {
        return Err(Error::Control("unknown qualification resource metric"));
    }
    Ok(())
}

pub(in crate::qualification) fn validate_resource_metric_units(
    metrics: &[QualificationMetric],
) -> Result<()> {
    for metric in metrics {
        if let Some((_, expected_unit)) = QUALIFICATION_RESOURCE_METRICS
            .iter()
            .find(|(name, _)| metric.name() == *name)
            && metric.unit() != *expected_unit
        {
            return Err(Error::Control("qualification resource metric unit"));
        }
    }
    Ok(())
}

pub(in crate::qualification) fn validate_run_latency_metrics(
    metrics: &[QualificationMetric],
) -> Result<()> {
    let mut latency = [0_u64; 4];
    for (index, (name, unit)) in [
        ("p50_latency_ms", "ms"),
        ("p95_latency_ms", "ms"),
        ("p99_latency_ms", "ms"),
        ("max_latency_ms", "ms"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut matches = metrics.iter().filter(|metric| metric.name() == name);
        let Some(metric) = matches.next() else {
            return Err(Error::Control("qualification run latency metrics"));
        };
        if matches.next().is_some() || metric.unit() != unit {
            return Err(Error::Control("qualification run latency metrics"));
        }
        latency[index] = metric.value();
    }
    if !latency.windows(2).all(|pair| pair[0] <= pair[1]) {
        return Err(Error::Control("qualification run latency percentile order"));
    }
    Ok(())
}
