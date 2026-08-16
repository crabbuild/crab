//! Provider-side inventory report consumers.
//!
//! Each provider has a different report format:
//!
//! - **S3 Inventory** — Parquet, ORC, or CSV.
//! - **GCS Storage Insights** — Parquet.
//! - **Azure Blob Inventory** — CSV.
//!
//! All parsers stream under bounded RAM to handle reports with
//! hundreds of millions of rows.

pub mod azure;
pub mod gcs;
pub mod s3;

/// Schema format of a provider-side inventory report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportSchema {
    /// Apache Parquet format (S3, GCS).
    Parquet,
    /// Apache ORC format (S3 only).
    Orc,
    /// CSV format (S3, Azure).
    Csv,
}

impl std::fmt::Display for ReportSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parquet => write!(f, "parquet"),
            Self::Orc => write!(f, "orc"),
            Self::Csv => write!(f, "csv"),
        }
    }
}

impl std::str::FromStr for ReportSchema {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "orc" => Self::Orc,
            "csv" => Self::Csv,
            _ => Self::Parquet,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_schema_from_str() {
        assert_eq!(
            "parquet".parse::<ReportSchema>().unwrap(),
            ReportSchema::Parquet
        );
        assert_eq!(
            "PARQUET".parse::<ReportSchema>().unwrap(),
            ReportSchema::Parquet
        );
        assert_eq!("orc".parse::<ReportSchema>().unwrap(), ReportSchema::Orc);
        assert_eq!("csv".parse::<ReportSchema>().unwrap(), ReportSchema::Csv);
        assert_eq!(
            "unknown".parse::<ReportSchema>().unwrap(),
            ReportSchema::Parquet
        );
    }

    #[test]
    fn report_schema_display() {
        assert_eq!(format!("{}", ReportSchema::Parquet), "parquet");
        assert_eq!(format!("{}", ReportSchema::Orc), "orc");
        assert_eq!(format!("{}", ReportSchema::Csv), "csv");
    }
}
