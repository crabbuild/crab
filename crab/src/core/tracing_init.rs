//! Tracing subscriber bootstrap.
//!
//! Installs a global `tracing_subscriber` before the tokio runtime is
//! built so that runtime-internal spans are captured. Auto-detects
//! whether stderr is a TTY: human-readable format for interactive use,
//! compact JSON for piped/redirected output.
//!
//! When compiled with `--features otlp` and the `CRAB_OTLP_ENDPOINT`
//! env var is set, an OpenTelemetry OTLP tracing layer is added that
//! exports spans to the configured collector endpoint.
//!
//! When compiled with `--features profiling` and `CRAB_PROFILE=1` is
//! set at runtime, a `tracing-flame` layer writes folded stack output
//! to `~/.cache/crab/profile/{session}.folded`.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::prelude::*;

/// Opaque guard whose `Drop` impl flushes the subscriber and shuts
/// down any OTLP pipeline. When profiling is active, dropping this
/// guard also flushes the flame-graph folded-stack writer.
///
/// Hold this in `main()` for the lifetime of the process.
pub struct TracingGuard {
    #[cfg(feature = "otlp")]
    provider: Option<opentelemetry_sdk::trace::TracerProvider>,
    #[cfg(feature = "profiling")]
    _flame_guard: Option<tracing_flame::FlushGuard<std::io::BufWriter<std::fs::File>>>,
}

/// The env var that controls the OTLP collector endpoint.
const OTLP_ENDPOINT_VAR: &str = "CRAB_OTLP_ENDPOINT";

/// The env var that enables flame-graph profiling output.
const PROFILE_VAR: &str = "CRAB_PROFILE";

/// Install the global tracing subscriber.
///
/// - If stderr is a TTY, uses a human-readable, colored format.
/// - Otherwise, uses compact JSON (one object per line).
/// - `cli_level` overrides the filter when present (from `--log-level`).
///   Otherwise the `CRAB_LOG` env var is used, falling back to `warn`.
/// - When the `otlp` feature is compiled in and `CRAB_OTLP_ENDPOINT`
///   is set, an OTLP exporter layer is added.
///
/// Must be called before the tokio runtime is built.
pub fn install_tracing_subscriber(cli_level: Option<&str>) -> TracingGuard {
    let env_filter = if let Some(level) = cli_level {
        // CLI flag takes highest priority.
        EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("warn"))
    } else {
        EnvFilter::try_from_env("CRAB_LOG").unwrap_or_else(|_| EnvFilter::new("warn"))
    };

    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());

    let fmt_layer = if is_tty {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_span_events(FmtSpan::CLOSE)
            .with_target(true)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(std::io::stderr)
            .with_span_events(FmtSpan::CLOSE)
            .with_target(true)
            .boxed()
    };

    #[cfg(feature = "otlp")]
    let (otel_layer, provider) = try_init_otlp();

    #[cfg(not(feature = "otlp"))]
    let otel_layer: Option<tracing_subscriber::layer::Identity> = {
        if std::env::var_os(OTLP_ENDPOINT_VAR).is_some() {
            eprintln!(
                "info: {OTLP_ENDPOINT_VAR} is set but OTLP export is \
                 unavailable (compile with --features otlp)"
            );
        }
        None
    };

    #[cfg(feature = "profiling")]
    let (flame_layer, flame_guard) = try_init_flame();

    #[cfg(not(feature = "profiling"))]
    let flame_layer: Option<tracing_subscriber::layer::Identity> = {
        if is_profiling_requested() {
            eprintln!(
                "info: {PROFILE_VAR}=1 is set but profiling is \
                 unavailable (compile with --features profiling)"
            );
        }
        None
    };

    // Build the subscriber with all layers. `Option<L>` implements
    // `Layer` (as a no-op when `None`), so optional layers are
    // transparently skipped when not configured.
    //
    // The flame layer is added first (closest to the Registry) because
    // `FlameLayer<Registry, W>` implements `Layer<Registry>` — it must
    // sit directly on the Registry before other layers wrap it.
    tracing_subscriber::registry()
        .with(flame_layer)
        .with(otel_layer)
        .with(env_filter)
        .with(fmt_layer)
        .init();

    #[cfg(not(feature = "otlp"))]
    if std::env::var_os(OTLP_ENDPOINT_VAR).is_some() {
        tracing::info!(
            "{OTLP_ENDPOINT_VAR} is set but OTLP export is \
             unavailable (compile with --features otlp)"
        );
    }

    #[cfg(not(feature = "profiling"))]
    if is_profiling_requested() {
        tracing::info!(
            "{PROFILE_VAR}=1 is set but profiling is \
             unavailable (compile with --features profiling)"
        );
    }

    TracingGuard {
        #[cfg(feature = "otlp")]
        provider,
        #[cfg(feature = "profiling")]
        _flame_guard: flame_guard,
    }
}

/// Build an optional OTLP tracing layer.
///
/// Returns `(Some(layer), Some(provider))` when `CRAB_OTLP_ENDPOINT`
/// is set and the pipeline initializes successfully, or `(None, None)`
/// otherwise.
#[cfg(feature = "otlp")]
fn try_init_otlp() -> (
    Option<
        tracing_opentelemetry::OpenTelemetryLayer<
            tracing_subscriber::Registry,
            opentelemetry_sdk::trace::Tracer,
        >,
    >,
    Option<opentelemetry_sdk::trace::TracerProvider>,
) {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig;

    let endpoint = match std::env::var(OTLP_ENDPOINT_VAR) {
        Ok(ep) if !ep.is_empty() => ep,
        _ => return (None, None),
    };

    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .build()
    {
        Ok(e) => e,
        Err(err) => {
            eprintln!("warn: failed to initialize OTLP exporter: {err}");
            return (None, None);
        }
    };

    // Simple exporter — the subscriber is installed before the tokio
    // runtime, so a batch exporter requiring a runtime handle is not
    // available yet. Fine for the low-volume top-level spans crab emits.
    let provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_simple_exporter(exporter)
        .build();

    let tracer = provider.tracer("crab");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);

    eprintln!("info: OTLP tracing enabled → {endpoint}");

    (Some(layer), Some(provider))
}

/// Returns `true` when the user has requested profiling via `CRAB_PROFILE=1`.
fn is_profiling_requested() -> bool {
    std::env::var(PROFILE_VAR).ok().is_some_and(|v| v == "1")
}

/// Build an optional flame-graph tracing layer.
///
/// When `CRAB_PROFILE=1` is set, creates the output directory
/// `~/.cache/crab/profile/` and writes folded stack traces to a
/// session-specific file. Returns `(Some(layer), Some(guard))` on
/// success, or `(None, None)` when profiling is not requested or
/// setup fails.
#[cfg(feature = "profiling")]
fn try_init_flame() -> (
    Option<
        tracing_flame::FlameLayer<tracing_subscriber::Registry, std::io::BufWriter<std::fs::File>>,
    >,
    Option<tracing_flame::FlushGuard<std::io::BufWriter<std::fs::File>>>,
) {
    if !is_profiling_requested() {
        return (None, None);
    }

    let profile_dir = crate::cache::default_cache_root().join("profile");
    if let Err(err) = std::fs::create_dir_all(&profile_dir) {
        eprintln!(
            "warn: failed to create profile directory {}: {err}",
            profile_dir.display()
        );
        return (None, None);
    }

    // Use process ID + timestamp for a simple, collision-resistant session name.
    let session = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    );

    let output_path = profile_dir.join(format!("{session}.folded"));

    match tracing_flame::FlameLayer::with_file(&output_path) {
        Ok((layer, guard)) => {
            eprintln!("info: profiling enabled → {}", output_path.display());
            (Some(layer), Some(guard))
        }
        Err(err) => {
            eprintln!("warn: failed to initialize flame layer: {err}");
            (None, None)
        }
    }
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        #[cfg(feature = "otlp")]
        if let Some(provider) = self.provider.take() {
            if let Err(err) = provider.shutdown() {
                eprintln!("warn: OTLP provider shutdown error: {err}");
            }
        }
    }
}
