//! Cross-cutting tracing helpers for gitoxide adoption.
//!
//! This module owns the span-naming convention that separates
//! gitoxide time from crab time in flamegraphs. Any crab
//! call site that dispatches into a `gix-*` crate wraps itself
//! in the [`gix_boundary!`] macro so profiling tools can attribute
//! CPU time to the gitoxide side of the adoption frontier.
//!
//! The macro is an enabler: declaring it here lets Phase 1 and
//! Phase 2 adoption PRs retrofit call sites one at a time without
//! each PR inventing its own span-naming scheme. No existing call
//! sites are retrofitted in the Phase 0 task that introduces this
//! module — that work happens per-requirement as each `gix-*` crate
//! comes online.
//!
//! # Example
//!
//! ```ignore
//! // In a call site about to dispatch into `gix_ref`:
//! let _span = crab::core::tracing::gix_boundary!("refs", "rev_parse").entered();
//! let oid = store.try_find_loose(...)?;
//! ```
//!
//! The span is emitted at `debug` level so it stays silent at
//! production log levels (which default to `error`). Flamegraph
//! tooling opts into `debug` explicitly.

/// Re-export of the `gix_boundary!` macro under this module path.
///
/// `#[macro_export]` places the macro at the crate root; this
/// re-export lets callers reach it as
/// `crate::core::tracing::gix_boundary!(...)` / `crab::core::tracing::gix_boundary!(...)`,
/// which is where the rest of `core::tracing` lives.
pub use crate::gix_boundary;

/// Build a `debug_span!` that marks a gitoxide call boundary.
///
/// Invoked as `gix_boundary!("<crate>", "<fn>")` where both arguments
/// are string literals. Expands to a `tracing::debug_span!` with:
///
/// - span name `"gix.<crate>.<fn>"` (built at compile time via `concat!`)
/// - structured fields `gix_crate = "<crate>"` and `gix_fn = "<fn>"`
///
/// The span is emitted at `debug` level so it stays silent at
/// production log levels; flamegraph tooling reads `debug` spans
/// explicitly. Allocating a new name per call is avoided — `concat!`
/// resolves the full string at compile time.
///
/// # Example
///
/// ```ignore
/// let _span = crab::core::tracing::gix_boundary!("refs", "rev_parse").entered();
/// // ...call into gix_ref here...
/// ```
#[macro_export]
macro_rules! gix_boundary {
    ($crate_name:literal, $fn_name:literal) => {
        ::tracing::debug_span!(
            concat!("gix.", $crate_name, ".", $fn_name),
            gix_crate = $crate_name,
            gix_fn = $fn_name,
        )
    };
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::Attributes;
    use tracing::{Id, Metadata, Subscriber};

    /// Captured record of a single `new_span` observation.
    #[derive(Clone, Debug, Default)]
    struct SpanRecord {
        name: String,
        level: String,
        fields: Vec<(String, String)>,
    }

    /// Field visitor that records every field name/value pair on a span
    /// creation. Values are stringified via `Debug` to stay agnostic
    /// about the underlying field type.
    struct FieldCollector<'a>(&'a mut Vec<(String, String)>);

    impl Visit for FieldCollector<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .push((field.name().to_string(), format!("{value:?}")));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            // `debug_span!` with a string literal renders without
            // quotes when visited as `&str`. Use this path where
            // available so assertions can compare bare values.
            self.0.push((field.name().to_string(), value.to_string()));
        }
    }

    /// Minimal subscriber that enables `debug`-level spans and records
    /// every `new_span` observation into a shared `Vec<SpanRecord>`.
    /// Event hooks are no-ops; we only care about span creation here.
    struct RecordingSubscriber {
        records: Arc<Mutex<Vec<SpanRecord>>>,
    }

    impl Subscriber for RecordingSubscriber {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.level() <= &tracing::Level::DEBUG
        }

        fn new_span(&self, attrs: &Attributes<'_>) -> Id {
            let metadata = attrs.metadata();
            let mut fields = Vec::new();
            attrs.record(&mut FieldCollector(&mut fields));
            self.records.lock().unwrap().push(SpanRecord {
                name: metadata.name().to_string(),
                level: metadata.level().to_string(),
                fields,
            });
            // Monotonically-increasing id keyed off record count.
            let id = self.records.lock().unwrap().len() as u64;
            Id::from_u64(id)
        }

        fn record(&self, _span: &Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
        fn event(&self, _event: &tracing::Event<'_>) {}
        fn enter(&self, _span: &Id) {}
        fn exit(&self, _span: &Id) {}
    }

    /// Install the recording subscriber for the duration of `f`.
    fn with_recording<F: FnOnce()>(f: F) -> Vec<SpanRecord> {
        let records: Arc<Mutex<Vec<SpanRecord>>> = Arc::new(Mutex::new(Vec::new()));
        let subscriber = RecordingSubscriber {
            records: records.clone(),
        };
        tracing::subscriber::with_default(subscriber, f);
        let guard = records.lock().unwrap();
        guard.clone()
    }

    #[test]
    fn gix_boundary_compiles_and_produces_a_span() {
        // Smoke test: the macro compiles in a `fn` body context and
        // returns something that `.entered()` accepts without panic.
        let _span = crate::core::tracing::gix_boundary!("refs", "rev_parse").entered();
    }

    #[test]
    fn gix_boundary_span_has_expected_name_and_fields() {
        let records = with_recording(|| {
            let _span = crate::core::tracing::gix_boundary!("refs", "rev_parse").entered();
        });

        assert_eq!(records.len(), 1, "expected exactly one span");
        let record = &records[0];

        assert_eq!(record.name, "gix.refs.rev_parse", "span name mismatch");
        assert_eq!(record.level, "DEBUG", "span should be debug level");

        let crate_field = record
            .fields
            .iter()
            .find(|(k, _)| k == "gix_crate")
            .expect("gix_crate field should be present");
        assert_eq!(crate_field.1, "refs");

        let fn_field = record
            .fields
            .iter()
            .find(|(k, _)| k == "gix_fn")
            .expect("gix_fn field should be present");
        assert_eq!(fn_field.1, "rev_parse");
    }

    #[test]
    fn gix_boundary_distinguishes_different_call_sites() {
        let records = with_recording(|| {
            let _a = crate::core::tracing::gix_boundary!("pack", "write").entered();
            let _b = crate::core::tracing::gix_boundary!("ref", "transaction").entered();
        });

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "gix.pack.write");
        assert_eq!(records[1].name, "gix.ref.transaction");
    }
}
