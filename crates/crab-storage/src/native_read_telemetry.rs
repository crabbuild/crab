use reqwest::Method;

pub(crate) const TARGET: &str = "crab_storage::native_read";

pub(crate) struct NativeReadObservation {
    provider: &'static str,
    method: &'static str,
    status: u16,
    bytes: u64,
}

impl NativeReadObservation {
    pub(crate) fn start(method: &Method, scheme: Option<&str>, host: Option<&str>) -> Option<Self> {
        if !tracing::enabled!(target: TARGET, tracing::Level::DEBUG) || scheme != Some("https") {
            return None;
        }
        let method = match *method {
            Method::GET => "get",
            Method::HEAD => "head",
            _ => return None,
        };
        Some(Self {
            provider: native_provider(host?)?,
            method,
            status: 0,
            bytes: 0,
        })
    }

    pub(crate) fn status(&mut self, status: u16) {
        self.status = status;
    }

    pub(crate) fn received(&mut self, bytes: usize) {
        self.bytes = self.bytes.saturating_add(bytes as u64);
    }
}

impl Drop for NativeReadObservation {
    fn drop(&mut self) {
        tracing::debug!(
            target: TARGET,
            parent: None,
            provider = self.provider,
            method = self.method,
            status = self.status,
            bytes = self.bytes,
            "Native object store read"
        );
    }
}

fn native_provider(host: &str) -> Option<&'static str> {
    // Credential refresh can share the provider's HTTP connector. Count only
    // canonical object-store endpoints and leave other traffic to the runner.
    if host == "storage.googleapis.com" {
        return Some("gcs");
    }
    if let Some(account) = host.strip_suffix(".blob.core.windows.net")
        && !account.is_empty()
        && account.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Some("azure");
    }
    let aws = host.strip_suffix(".amazonaws.com")?;
    let (service, region) = aws.rsplit_once('.').unwrap_or(("", aws));
    if region == "s3"
        || (region.starts_with(|character: char| character.is_ascii_lowercase())
            && region.contains('-')
            && region.ends_with(|character: char| character.is_ascii_digit())
            && service.rsplit('.').next() == Some("s3"))
    {
        Some("s3")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_backend_meter_excludes_credential_and_cdn_hosts() {
        for (host, expected) in [
            ("bucket.s3.us-west-2.amazonaws.com", Some("s3")),
            ("bucket.s3.amazonaws.com", Some("s3")),
            ("s3.us-east-1.amazonaws.com", Some("s3")),
            ("storage.googleapis.com", Some("gcs")),
            ("account.blob.core.windows.net", Some("azure")),
            ("sts.us-east-1.amazonaws.com", None),
            ("oauth2.googleapis.com", None),
            ("169.254.169.254", None),
            ("downloads.cloudfront.net", None),
        ] {
            assert_eq!(native_provider(host), expected, "{host}");
        }
    }
}
