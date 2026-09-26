use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use tracing::{Event, Subscriber, field::Visit, span};
use tracing_subscriber::{Layer, layer::Context, prelude::*, registry::LookupSpan};

#[derive(Clone, Default)]
pub(super) struct Events(Arc<Mutex<Vec<Map<String, Value>>>>);

#[derive(Default)]
struct Fields(Map<String, Value>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().into(), format!("{value:?}").into());
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().into(), value.into());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.insert(field.name().into(), value.into());
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Events {
    fn on_new_span(
        &self,
        attributes: &span::Attributes<'_>,
        id: &span::Id,
        context: Context<'_, S>,
    ) {
        let mut fields = Fields::default();
        attributes.record(&mut fields);
        context.span(id).unwrap().extensions_mut().insert(fields);
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, context: Context<'_, S>) {
        let span = context.span(id).unwrap();
        let mut extensions = span.extensions_mut();
        values.record(extensions.get_mut::<Fields>().unwrap());
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let mut fields = Fields::default();
        if let Some(scope) = context.event_scope(event) {
            for span in scope.from_root() {
                if let Some(parent) = span.extensions().get::<Fields>() {
                    fields.0.extend(parent.0.clone());
                }
            }
        }
        event.record(&mut fields);
        if fields.0.contains_key("event") {
            let mut events = self.0.lock().unwrap();
            assert!(
                events.len() < 10_000,
                "trace fixture exceeded its event bound"
            );
            events.push(fields.0);
        }
    }
}

impl Events {
    pub(super) fn install() -> Self {
        let events = Self::default();
        let filter = tracing_subscriber::filter::Targets::new()
            .with_target("crab_cell_runtime::action", tracing::Level::DEBUG)
            .with_target("crab_http_server::action", tracing::Level::DEBUG)
            .with_target("crab_http_server::server", tracing::Level::INFO);
        let subscriber = tracing_subscriber::registry()
            .with(events.clone().with_filter(filter.clone()))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_target(false)
                    .with_filter(filter),
            );
        tracing::subscriber::set_global_default(subscriber)
            .expect("run the RustFS tracing qualifier in an isolated test process");
        events
    }

    pub(super) fn verify_acknowledgement(&self, request_id: &str) {
        let events = self.0.lock().unwrap();
        let http = events
            .iter()
            .filter(|event| event.get("request_id") == Some(&request_id.into()));
        let invocation = http
            .clone()
            .find(|event| event["event"] == "cell_invocation_completed")
            .unwrap();
        assert_eq!(invocation["outcome"], "committed");
        let submission = http
            .clone()
            .find(|event| event["event"] == "application_submission")
            .unwrap();
        assert_eq!(
            submission["submission_id"],
            "00000000-0000-4000-8000-000000000001"
        );
        let response = http
            .clone()
            .find(|event| event["event"] == "http_response_ready")
            .unwrap();
        assert_eq!(response["status"], 201);
        let owner = events
            .iter()
            .filter(|event| {
                event.get("mutation_request_id") == invocation.get("mutation_request_id")
                    && event.get("cell") == invocation.get("cell")
                    && event.get("incarnation") == invocation.get("incarnation")
                    && event.contains_key("owner_session")
            })
            .collect::<Vec<_>>();
        let released = owner
            .iter()
            .find(|event| event["event"] == "cell_command_response")
            .unwrap();
        assert_eq!(released["commit_sequence"], invocation["commit_sequence"]);
        assert!(matches!(
            released["source"].as_str(),
            Some("Object" | "Fleet" | "Recorded")
        ));
        for name in [
            "cell_execution_started",
            "cell_worker_started",
            "cell_worker_completed",
            "cell_capture_completed",
            "cell_execution_completed",
            "cell_proof_completed",
        ] {
            assert!(
                owner.iter().any(|event| event["event"] == name),
                "missing {name} for {invocation:?}"
            );
        }
        for event in http.chain(owner) {
            eprintln!("action-trace {}", serde_json::to_string(event).unwrap());
        }
    }
}
