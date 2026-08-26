//! Bounded-cardinality metrics for the Git LFS HTTP gateway.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::http::StatusCode;

/// Process-local counters for gateway traffic and saturation.
#[derive(Debug, Default)]
pub struct LfsMetrics {
    requests_total: AtomicU64,
    responses_1xx: AtomicU64,
    responses_2xx: AtomicU64,
    responses_3xx: AtomicU64,
    responses_4xx: AtomicU64,
    responses_5xx: AtomicU64,
    request_duration_ms_total: AtomicU64,
    request_bytes_total: AtomicU64,
    response_bytes_total: AtomicU64,
    active_requests: AtomicU64,
    rate_limited_total: AtomicU64,
    spool_rejections_total: AtomicU64,
}

impl LfsMetrics {
    /// Records that a request entered the gateway middleware.
    pub fn request_started(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.active_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a request rejected by process-local rate admission.
    pub fn rate_limited(&self) {
        self.rate_limited_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Records an upload rejected by the aggregate spool-byte budget.
    pub fn spool_rejected(&self) {
        self.spool_rejections_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Records the response observed by the HTTP middleware.
    pub fn request_finished(
        &self,
        status: StatusCode,
        elapsed: Duration,
        request_bytes: Option<u64>,
        response_bytes: Option<u64>,
    ) {
        let counter = match status.as_u16() / 100 {
            1 => &self.responses_1xx,
            2 => &self.responses_2xx,
            3 => &self.responses_3xx,
            4 => &self.responses_4xx,
            5 => &self.responses_5xx,
            _ => {
                self.active_requests.fetch_sub(1, Ordering::Relaxed);
                return;
            }
        };
        counter.fetch_add(1, Ordering::Relaxed);
        let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        self.request_duration_ms_total
            .fetch_add(duration_ms, Ordering::Relaxed);
        if let Some(bytes) = request_bytes {
            self.request_bytes_total.fetch_add(bytes, Ordering::Relaxed);
        }
        if let Some(bytes) = response_bytes {
            self.response_bytes_total
                .fetch_add(bytes, Ordering::Relaxed);
        }
        self.active_requests.fetch_sub(1, Ordering::Relaxed);
    }

    /// Renders Prometheus text exposition with only process-wide labels.
    #[must_use]
    pub fn render(&self) -> String {
        let mut body = String::with_capacity(1_200);
        append_counter(
            &mut body,
            "crab_lfs_http_requests_total",
            "HTTP requests observed by the Crab LFS gateway",
            self.requests_total.load(Ordering::Relaxed),
        );
        append_metric_header(
            &mut body,
            "crab_lfs_http_responses_total",
            "HTTP responses by status class",
            "counter",
        );
        append_metric_sample(
            &mut body,
            "crab_lfs_http_responses_total{class=\"1xx\"}",
            self.responses_1xx.load(Ordering::Relaxed),
        );
        append_metric_sample(
            &mut body,
            "crab_lfs_http_responses_total{class=\"2xx\"}",
            self.responses_2xx.load(Ordering::Relaxed),
        );
        append_metric_sample(
            &mut body,
            "crab_lfs_http_responses_total{class=\"3xx\"}",
            self.responses_3xx.load(Ordering::Relaxed),
        );
        append_metric_sample(
            &mut body,
            "crab_lfs_http_responses_total{class=\"4xx\"}",
            self.responses_4xx.load(Ordering::Relaxed),
        );
        append_metric_sample(
            &mut body,
            "crab_lfs_http_responses_total{class=\"5xx\"}",
            self.responses_5xx.load(Ordering::Relaxed),
        );
        append_counter(
            &mut body,
            "crab_lfs_http_request_duration_ms_total",
            "Observed HTTP handler latency in milliseconds",
            self.request_duration_ms_total.load(Ordering::Relaxed),
        );
        append_counter(
            &mut body,
            "crab_lfs_http_request_bytes_total",
            "Request bytes reported by Content-Length",
            self.request_bytes_total.load(Ordering::Relaxed),
        );
        append_counter(
            &mut body,
            "crab_lfs_http_response_bytes_total",
            "Response bytes reported by Content-Length",
            self.response_bytes_total.load(Ordering::Relaxed),
        );
        append_gauge(
            &mut body,
            "crab_lfs_http_active_requests",
            "HTTP requests currently inside the gateway middleware",
            self.active_requests.load(Ordering::Relaxed),
        );
        append_counter(
            &mut body,
            "crab_lfs_http_rate_limited_total",
            "Requests rejected by process-local rate admission",
            self.rate_limited_total.load(Ordering::Relaxed),
        );
        append_counter(
            &mut body,
            "crab_lfs_http_spool_rejections_total",
            "Uploads rejected by aggregate spool-byte admission",
            self.spool_rejections_total.load(Ordering::Relaxed),
        );
        body
    }
}

fn append_counter(body: &mut String, name: &str, help: &str, value: u64) {
    append_metric_header(body, name, help, "counter");
    append_metric_sample(body, name, value);
}

fn append_metric_header(body: &mut String, name: &str, help: &str, kind: &str) {
    body.push_str("# HELP ");
    body.push_str(name);
    body.push(' ');
    body.push_str(help);
    body.push('\n');
    body.push_str("# TYPE ");
    body.push_str(name);
    body.push(' ');
    body.push_str(kind);
    body.push('\n');
}

fn append_metric_sample(body: &mut String, name: &str, value: u64) {
    body.push_str(name);
    body.push(' ');
    body.push_str(&value.to_string());
    body.push('\n');
}

fn append_gauge(body: &mut String, name: &str, help: &str, value: u64) {
    append_metric_header(body, name, help, "gauge");
    append_metric_sample(body, name, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_exposes_bounded_status_metrics() {
        let metrics = LfsMetrics::default();
        metrics.request_started();
        metrics.request_finished(StatusCode::OK, Duration::from_millis(7), Some(11), Some(13));
        metrics.request_started();
        metrics.request_finished(
            StatusCode::BAD_REQUEST,
            Duration::from_millis(3),
            None,
            None,
        );

        let rendered = metrics.render();
        assert!(rendered.contains("crab_lfs_http_requests_total 2"));
        assert!(rendered.contains("crab_lfs_http_responses_total{class=\"2xx\"} 1"));
        assert!(rendered.contains("crab_lfs_http_responses_total{class=\"4xx\"} 1"));
        assert!(rendered.contains("crab_lfs_http_request_duration_ms_total 10"));
        assert!(rendered.contains("crab_lfs_http_active_requests 0"));
        metrics.rate_limited();
        metrics.spool_rejected();
        let rendered = metrics.render();
        assert!(rendered.contains("crab_lfs_http_rate_limited_total 1"));
        assert!(rendered.contains("crab_lfs_http_spool_rejections_total 1"));
    }
}
