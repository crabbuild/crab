use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) enum Status {
    Pending,
    Success,
    Failure,
}

impl Status {
    pub(super) fn parse(value: &str) -> std::result::Result<Self, &'static str> {
        match value.to_ascii_lowercase().as_str() {
            "pending" => Ok(Self::Pending),
            "success" => Ok(Self::Success),
            "failure" => Ok(Self::Failure),
            _ => Err("Search status must be pending, success, or failure"),
        }
    }

    pub(super) fn predicate(self) -> &'static str {
        match self {
            Self::Failure => "COALESCE(ci.failed,0) > 0",
            Self::Pending => "COALESCE(ci.failed,0)=0 AND (ci.oid IS NULL OR ci.pending > 0)",
            Self::Success => "ci.oid IS NOT NULL AND ci.failed=0 AND ci.pending=0",
        }
    }
}
