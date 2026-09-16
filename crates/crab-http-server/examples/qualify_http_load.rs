use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clap::Parser;
use futures_util::StreamExt as _;
use reqwest::{
    Client, StatusCode,
    header::{CONTENT_LENGTH, HOST, HeaderMap, HeaderName, HeaderValue, TRANSFER_ENCODING},
};
use serde::Serialize;
use tokio::{sync::Barrier, task::JoinSet};
use url::Url;

const REPORT_SCHEMA: u32 = 1;
const MAX_HEADERS: usize = 64;
const MAX_HEADER_FILE_BYTES: u64 = 64 * 1024;
const MAX_TARGETS: usize = 64;
const MAX_TOTAL_CONCURRENCY: usize = 4_096;
const MAX_LATENCY_MS: u64 = 60_000;

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("invalid load qualification configuration: {0}")]
    Configuration(&'static str),
    #[error("failed to read load qualification header file")]
    HeaderFile(#[source] io::Error),
    #[error("invalid HTTP header in load qualification header file")]
    Header,
    #[error("invalid load qualification URL")]
    Url(#[source] url::ParseError),
    #[error("load qualification HTTP request failed")]
    Http(#[source] reqwest::Error),
    #[error("load qualification health response body failed")]
    HealthBody(#[source] BodyError),
    #[error("load qualification worker failed")]
    Worker(#[source] tokio::task::JoinError),
    #[error("failed to encode load qualification report")]
    Encode(#[source] serde_json::Error),
    #[error("failed to write load qualification report")]
    Output(#[source] io::Error),
    #[error("system clock predates the Unix epoch")]
    Clock,
    #[error("load qualification observed an unhealthy or invalid response")]
    Qualification,
}

#[derive(Debug, Parser)]
#[command(about = "Generate bounded crab-http-server HTTP load evidence")]
struct Arguments {
    /// Public server origin, without a path, query, or embedded credentials.
    #[arg(long)]
    base_url: Url,

    /// Repeated NAME=CONCURRENCY@/PATH targets run concurrently.
    #[arg(
        long = "target",
        required = true,
        value_name = "NAME=CONCURRENCY@/PATH"
    )]
    targets: Vec<TargetSpec>,

    /// Measured duration after all workers finish warmup.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3_600))]
    duration_seconds: u64,

    /// Unmeasured warmup duration for every worker.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u64).range(0..=300))]
    warmup_seconds: u64,

    /// Optional file containing one Authorization, Cookie, or other header per line.
    #[arg(long)]
    header_file: Option<PathBuf>,

    /// Health path checked before and after measured traffic.
    #[arg(long, default_value = "/livez")]
    health_path: String,

    /// Abort a response body after this many bytes.
    #[arg(
        long,
        default_value_t = 64 * 1024 * 1024,
        value_parser = clap::value_parser!(u64).range(1..=1_073_741_824)
    )]
    max_response_bytes: u64,
}

#[derive(Clone, Debug)]
struct TargetSpec {
    name: String,
    concurrency: usize,
    path: String,
}

impl FromStr for TargetSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (name, request) = value
            .split_once('=')
            .ok_or_else(|| "target must contain '='".to_owned())?;
        let (concurrency, path) = request
            .split_once('@')
            .ok_or_else(|| "target must contain '@'".to_owned())?;
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("target name must be 1-64 ASCII letters, digits, '-' or '_'".to_owned());
        }
        let concurrency = concurrency
            .parse::<usize>()
            .map_err(|_| "target concurrency is not an integer".to_owned())?;
        if !(1..=1_024).contains(&concurrency) {
            return Err("target concurrency must be between 1 and 1024".to_owned());
        }
        if !path.starts_with('/')
            || path.starts_with("//")
            || path.contains('#')
            || path.chars().any(char::is_control)
        {
            return Err(
                "target path must be one absolute-origin path without a fragment".to_owned(),
            );
        }
        Ok(Self {
            name: name.to_owned(),
            concurrency,
            path: path.to_owned(),
        })
    }
}

#[derive(Clone)]
struct ResolvedTarget {
    spec: TargetSpec,
    url: Url,
}

#[derive(Default)]
struct WorkerStats {
    responses: u64,
    successful_responses: u64,
    admission_rejections: u64,
    unexpected_responses: u64,
    server_errors: u64,
    transport_errors: u64,
    body_limit_errors: u64,
    response_bytes: u64,
    latency: Histogram,
    successful_latency: Histogram,
    elapsed: Duration,
}

impl WorkerStats {
    fn record(&mut self, outcome: RequestOutcome) {
        match outcome {
            RequestOutcome::Complete {
                status,
                bytes,
                elapsed,
            } => {
                self.responses = self.responses.saturating_add(1);
                self.response_bytes = self.response_bytes.saturating_add(bytes);
                self.latency.record(elapsed);
                if status.is_success() {
                    self.successful_responses = self.successful_responses.saturating_add(1);
                    self.successful_latency.record(elapsed);
                } else if status == StatusCode::TOO_MANY_REQUESTS {
                    self.admission_rejections = self.admission_rejections.saturating_add(1);
                } else {
                    self.unexpected_responses = self.unexpected_responses.saturating_add(1);
                    if status.is_server_error() {
                        self.server_errors = self.server_errors.saturating_add(1);
                    }
                }
            }
            RequestOutcome::Transport => {
                self.transport_errors = self.transport_errors.saturating_add(1);
            }
            RequestOutcome::BodyLimit => {
                self.body_limit_errors = self.body_limit_errors.saturating_add(1);
            }
        }
    }

    fn merge(&mut self, other: Self) {
        self.responses = self.responses.saturating_add(other.responses);
        self.successful_responses = self
            .successful_responses
            .saturating_add(other.successful_responses);
        self.admission_rejections = self
            .admission_rejections
            .saturating_add(other.admission_rejections);
        self.unexpected_responses = self
            .unexpected_responses
            .saturating_add(other.unexpected_responses);
        self.server_errors = self.server_errors.saturating_add(other.server_errors);
        self.transport_errors = self.transport_errors.saturating_add(other.transport_errors);
        self.body_limit_errors = self
            .body_limit_errors
            .saturating_add(other.body_limit_errors);
        self.response_bytes = self.response_bytes.saturating_add(other.response_bytes);
        self.latency.merge(other.latency);
        self.successful_latency.merge(other.successful_latency);
        self.elapsed = self.elapsed.max(other.elapsed);
    }

    fn qualified(&self) -> bool {
        self.unexpected_responses == 0
            && self.server_errors == 0
            && self.transport_errors == 0
            && self.body_limit_errors == 0
    }
}

enum RequestOutcome {
    Complete {
        status: StatusCode,
        bytes: u64,
        elapsed: Duration,
    },
    Transport,
    BodyLimit,
}

#[derive(Debug, thiserror::Error)]
enum BodyError {
    #[error("HTTP response body transport failed")]
    Transport(#[source] reqwest::Error),
    #[error("HTTP response body exceeded its configured byte limit")]
    Limit,
}

#[derive(Default)]
struct Histogram {
    buckets: BTreeMap<u64, u64>,
    count: u64,
    maximum_ms: u64,
}

impl Histogram {
    fn record(&mut self, elapsed: Duration) {
        let millis = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let bucket = millis.min(MAX_LATENCY_MS + 1);
        let count = self.buckets.entry(bucket).or_default();
        *count = count.saturating_add(1);
        self.count = self.count.saturating_add(1);
        self.maximum_ms = self.maximum_ms.max(millis);
    }

    fn merge(&mut self, other: Self) {
        for (millis, incoming) in other.buckets {
            let current = self.buckets.entry(millis).or_default();
            *current = current.saturating_add(incoming);
        }
        self.count = self.count.saturating_add(other.count);
        self.maximum_ms = self.maximum_ms.max(other.maximum_ms);
    }

    fn percentile(&self, numerator: u64, denominator: u64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let rank = self
            .count
            .saturating_mul(numerator)
            .saturating_add(denominator.saturating_sub(1))
            / denominator;
        let mut observed = 0u64;
        for (millis, count) in &self.buckets {
            observed = observed.saturating_add(*count);
            if observed >= rank {
                return Some(*millis);
            }
        }
        Some(MAX_LATENCY_MS + 1)
    }

    fn summary(&self) -> LatencyReport {
        LatencyReport {
            p50_ms: self.percentile(50, 100),
            p95_ms: self.percentile(95, 100),
            p99_ms: self.percentile(99, 100),
            max_ms: (self.count != 0).then_some(self.maximum_ms),
            over_60s: self
                .buckets
                .get(&(MAX_LATENCY_MS + 1))
                .copied()
                .unwrap_or(0),
        }
    }
}

#[derive(Serialize)]
struct QualificationReport {
    schema_version: u32,
    started_at_ms: u64,
    base_url: String,
    configured_duration_ms: u64,
    warmup_ms: u64,
    max_response_bytes: u64,
    health_before: HealthReport,
    health_after: HealthReport,
    targets: Vec<TargetReport>,
    aggregate: TrafficReport,
    qualified: bool,
}

#[derive(Serialize)]
struct HealthReport {
    status: u16,
    latency_ms: u64,
    response_bytes: u64,
}

#[derive(Serialize)]
struct TargetReport {
    name: String,
    path: String,
    concurrency: usize,
    #[serde(flatten)]
    traffic: TrafficReport,
}

#[derive(Serialize)]
struct TrafficReport {
    elapsed_ms: u64,
    responses: u64,
    successful_responses: u64,
    admission_rejections: u64,
    unexpected_responses: u64,
    server_errors: u64,
    transport_errors: u64,
    body_limit_errors: u64,
    response_bytes: u64,
    responses_per_second: f64,
    successful_responses_per_second: f64,
    admission_rejection_percent: f64,
    latency: LatencyReport,
    successful_latency: LatencyReport,
}

impl TrafficReport {
    fn from_stats(stats: &WorkerStats) -> Self {
        let elapsed_seconds = stats.elapsed.as_secs_f64();
        Self {
            elapsed_ms: duration_millis(stats.elapsed),
            responses: stats.responses,
            successful_responses: stats.successful_responses,
            admission_rejections: stats.admission_rejections,
            unexpected_responses: stats.unexpected_responses,
            server_errors: stats.server_errors,
            transport_errors: stats.transport_errors,
            body_limit_errors: stats.body_limit_errors,
            response_bytes: stats.response_bytes,
            responses_per_second: if elapsed_seconds == 0.0 {
                0.0
            } else {
                stats.responses as f64 / elapsed_seconds
            },
            successful_responses_per_second: if elapsed_seconds == 0.0 {
                0.0
            } else {
                stats.successful_responses as f64 / elapsed_seconds
            },
            admission_rejection_percent: if stats.responses == 0 {
                0.0
            } else {
                stats.admission_rejections as f64 * 100.0 / stats.responses as f64
            },
            latency: stats.latency.summary(),
            successful_latency: stats.successful_latency.summary(),
        }
    }
}

#[derive(Serialize)]
struct LatencyReport {
    p50_ms: Option<u64>,
    p95_ms: Option<u64>,
    p99_ms: Option<u64>,
    max_ms: Option<u64>,
    over_60s: u64,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let arguments = Arguments::parse();
    validate_origin(&arguments.base_url)?;
    validate_path(&arguments.health_path)?;
    validate_targets(&arguments.targets)?;
    let headers = load_headers(arguments.header_file.as_deref())?;
    let total_concurrency = arguments
        .targets
        .iter()
        .map(|target| target.concurrency)
        .sum::<usize>();
    let client = Client::builder()
        .default_headers(headers)
        .pool_max_idle_per_host(total_concurrency)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(Error::Http)?;
    let health_url = arguments
        .base_url
        .join(&arguments.health_path)
        .map_err(Error::Url)?;
    let targets = arguments
        .targets
        .iter()
        .cloned()
        .map(|spec| {
            let url = arguments.base_url.join(&spec.path).map_err(Error::Url)?;
            Ok(ResolvedTarget { spec, url })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let started_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Clock)?
        .as_millis()
        .try_into()
        .map_err(|_| Error::Clock)?;
    let health_before = check_health(&client, &health_url, arguments.max_response_bytes).await?;
    let mut stats = run_load(
        client.clone(),
        &targets,
        Duration::from_secs(arguments.warmup_seconds),
        Duration::from_secs(arguments.duration_seconds),
        arguments.max_response_bytes,
    )
    .await?;
    let health_after = check_health(&client, &health_url, arguments.max_response_bytes).await?;
    let mut aggregate = WorkerStats::default();
    let target_reports = targets
        .into_iter()
        .zip(stats.drain(..))
        .map(|(target, target_stats)| {
            let traffic = TrafficReport::from_stats(&target_stats);
            aggregate.merge(target_stats);
            TargetReport {
                name: target.spec.name,
                path: target.spec.path,
                concurrency: target.spec.concurrency,
                traffic,
            }
        })
        .collect::<Vec<_>>();
    let qualified = health_before.status == StatusCode::OK.as_u16()
        && health_after.status == StatusCode::OK.as_u16()
        && aggregate.qualified();
    let report = QualificationReport {
        schema_version: REPORT_SCHEMA,
        started_at_ms,
        base_url: arguments.base_url.to_string(),
        configured_duration_ms: arguments.duration_seconds.saturating_mul(1_000),
        warmup_ms: arguments.warmup_seconds.saturating_mul(1_000),
        max_response_bytes: arguments.max_response_bytes,
        health_before,
        health_after,
        targets: target_reports,
        aggregate: TrafficReport::from_stats(&aggregate),
        qualified,
    };
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, &report).map_err(Error::Encode)?;
    writeln!(output).map_err(Error::Output)?;
    if !qualified {
        return Err(Error::Qualification);
    }
    Ok(())
}

fn validate_origin(url: &Url) -> Result<(), Error> {
    if !matches!(url.scheme(), "http" | "https")
        || url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Configuration(
            "base URL must be one HTTP(S) origin without credentials, path, query, or fragment",
        ));
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), Error> {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('#')
        || path.chars().any(char::is_control)
    {
        return Err(Error::Configuration(
            "health path must be one absolute-origin path without a fragment",
        ));
    }
    Ok(())
}

fn validate_targets(targets: &[TargetSpec]) -> Result<(), Error> {
    if targets.is_empty() || targets.len() > MAX_TARGETS {
        return Err(Error::Configuration("target count is outside its bound"));
    }
    let mut names = HashSet::with_capacity(targets.len());
    let mut total = 0usize;
    for target in targets {
        if !names.insert(target.name.as_str()) {
            return Err(Error::Configuration("target names must be unique"));
        }
        total = total
            .checked_add(target.concurrency)
            .ok_or(Error::Configuration("total concurrency overflow"))?;
    }
    if total > MAX_TOTAL_CONCURRENCY {
        return Err(Error::Configuration(
            "total target concurrency exceeds 4096",
        ));
    }
    Ok(())
}

fn load_headers(path: Option<&Path>) -> Result<HeaderMap, Error> {
    let Some(path) = path else {
        return Ok(HeaderMap::new());
    };
    let metadata = fs::metadata(path).map_err(Error::HeaderFile)?;
    if metadata.len() > MAX_HEADER_FILE_BYTES {
        return Err(Error::Configuration("header file exceeds 64 KiB"));
    }
    let body = fs::read_to_string(path).map_err(Error::HeaderFile)?;
    let mut headers = HeaderMap::new();
    for line in body.lines().filter(|line| !line.trim().is_empty()) {
        if headers.len() == MAX_HEADERS {
            return Err(Error::Configuration("header file exceeds 64 headers"));
        }
        let (name, value) = line.split_once(':').ok_or(Error::Header)?;
        let name = HeaderName::from_bytes(name.trim().as_bytes()).map_err(|_| Error::Header)?;
        if matches!(name, HOST | CONTENT_LENGTH | TRANSFER_ENCODING) {
            return Err(Error::Configuration(
                "header file cannot override HTTP framing or authority",
            ));
        }
        let value = HeaderValue::from_str(value.trim()).map_err(|_| Error::Header)?;
        headers.append(name, value);
    }
    Ok(headers)
}

async fn check_health(client: &Client, url: &Url, max_bytes: u64) -> Result<HealthReport, Error> {
    let started = Instant::now();
    let response = client.get(url.clone()).send().await.map_err(Error::Http)?;
    let status = response.status();
    let bytes = consume_body(response, max_bytes)
        .await
        .map_err(Error::HealthBody)?;
    Ok(HealthReport {
        status: status.as_u16(),
        latency_ms: duration_millis(started.elapsed()),
        response_bytes: bytes,
    })
}

async fn run_load(
    client: Client,
    targets: &[ResolvedTarget],
    warmup: Duration,
    duration: Duration,
    max_bytes: u64,
) -> Result<Vec<WorkerStats>, Error> {
    let workers = targets
        .iter()
        .map(|target| target.spec.concurrency)
        .sum::<usize>();
    let barrier = Arc::new(Barrier::new(workers));
    let mut tasks = JoinSet::new();
    for (index, target) in targets.iter().enumerate() {
        for _ in 0..target.spec.concurrency {
            tasks.spawn(run_worker(
                index,
                client.clone(),
                target.url.clone(),
                Arc::clone(&barrier),
                warmup,
                duration,
                max_bytes,
            ));
        }
    }
    let mut stats = (0..targets.len())
        .map(|_| WorkerStats::default())
        .collect::<Vec<_>>();
    while let Some(joined) = tasks.join_next().await {
        let (index, worker) = joined.map_err(Error::Worker)?;
        stats[index].merge(worker);
    }
    Ok(stats)
}

async fn run_worker(
    index: usize,
    client: Client,
    url: Url,
    barrier: Arc<Barrier>,
    warmup: Duration,
    duration: Duration,
    max_bytes: u64,
) -> (usize, WorkerStats) {
    barrier.wait().await;
    let warmup_deadline = Instant::now() + warmup;
    while Instant::now() < warmup_deadline {
        let _ = request(&client, &url, max_bytes).await;
    }
    barrier.wait().await;
    let started = Instant::now();
    let deadline = started + duration;
    let mut stats = WorkerStats::default();
    while Instant::now() < deadline {
        stats.record(request(&client, &url, max_bytes).await);
    }
    stats.elapsed = started.elapsed();
    (index, stats)
}

async fn request(client: &Client, url: &Url, max_bytes: u64) -> RequestOutcome {
    let started = Instant::now();
    let response = match client.get(url.clone()).send().await {
        Ok(response) => response,
        Err(_) => return RequestOutcome::Transport,
    };
    let status = response.status();
    let bytes = match consume_body(response, max_bytes).await {
        Ok(bytes) => bytes,
        Err(BodyError::Transport(_)) => return RequestOutcome::Transport,
        Err(BodyError::Limit) => return RequestOutcome::BodyLimit,
    };
    RequestOutcome::Complete {
        status,
        bytes,
        elapsed: started.elapsed(),
    }
}

async fn consume_body(response: reqwest::Response, max_bytes: u64) -> Result<u64, BodyError> {
    let mut bytes = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(BodyError::Transport)?;
        bytes = bytes
            .checked_add(chunk.len() as u64)
            .ok_or(BodyError::Limit)?;
        if bytes > max_bytes {
            return Err(BodyError::Limit);
        }
    }
    Ok(bytes)
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "qualify_http_load/tests.rs"]
mod tests;
