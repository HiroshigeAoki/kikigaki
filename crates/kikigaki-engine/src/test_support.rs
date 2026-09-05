//! Test-only helpers shared by this crate's unit tests.

use std::sync::{Arc, Mutex};

/// Runs `run` while capturing any `tracing::warn!` events emitted during the call.
///
/// Returns `run`'s result alongside the captured warnings, one per line in emission order. Each
/// line renders every field of its event (including `message`), so callers can assert on either
/// the message text or structured fields attached with `tracing::warn!(field = value, "...")`.
pub(crate) fn capture_warnings<T>(run: impl FnOnce() -> T) -> (T, String) {
    let messages = Arc::new(Mutex::new(Vec::new()));
    let subscriber = WarningSubscriber(Arc::clone(&messages));
    let result = tracing::subscriber::with_default(subscriber, run);
    let output = messages.lock().unwrap().join("\n");
    (result, output)
}

struct WarningSubscriber(Arc<Mutex<Vec<String>>>);

impl tracing::Subscriber for WarningSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        struct Visitor(String);
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?} ", field.name()));
            }
        }
        let mut visitor = Visitor(String::new());
        event.record(&mut visitor);
        self.0.lock().unwrap().push(visitor.0);
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}
