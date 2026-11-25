//! Optional Sentry reporting for the serving daemon. Reports use compiler
//! metadata and fixed messages. Raw tracing fields can contain private data.

use std::sync::Arc;
use std::time::Duration;

use sentry::protocol::{Event, Exception, Frame, Level, Stacktrace};
use tracing::field::{Field, Visit};
use tracing::{Event as TracingEvent, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

#[derive(Clone)]
pub(crate) struct Reporter(Arc<sentry::Client>);

impl Reporter {
    pub(crate) fn from_env(serving: bool) -> Option<Self> {
        if !serving {
            return None;
        }
        let dsn = std::env::var("INTENT_ERROR_REPORTING_DSN").ok()?;
        // The DSN is only used by the daemon. Agent subprocesses inherit the
        // process environment, so avoid exporting its project key to them.
        std::env::remove_var("INTENT_ERROR_REPORTING_DSN");
        let dsn = dsn.trim();
        if dsn.is_empty()
            || intent_core::process_policy::ProcessPolicy::current().private_test_profile()
        {
            return None;
        }
        if !dsn.starts_with("https://") {
            eprintln!("WARN: Intent error reporting requires an HTTPS Sentry DSN");
            return None;
        }
        let Ok(dsn) = dsn.parse() else {
            eprintln!("WARN: Invalid Sentry DSN for Intent error reporting");
            return None;
        };
        Some(Self(Arc::new(sentry::Client::from(
            sentry::ClientOptions {
                dsn: Some(dsn),
                transport: Some(Arc::new(sentry::transports::DefaultTransportFactory)),
                shutdown_timeout: Duration::from_secs(2),
                default_integrations: false,
                send_default_pii: false,
                ..Default::default()
            },
        ))))
    }

    pub(crate) fn flush(&self) {
        let _ = self.0.flush(Some(Duration::from_secs(2)));
    }
}

pub(crate) fn should_report(metadata: &Metadata<'_>) -> bool {
    let target = metadata.target();
    metadata.is_event()
        && *metadata.level() == tracing::Level::ERROR
        && (target == "intentd" || target.starts_with("intentd::") || target.starts_with("intent_"))
}

impl<S: Subscriber> Layer<S> for Reporter {
    fn on_event(&self, event: &TracingEvent<'_>, _: Context<'_, S>) {
        self.0.capture_event(report(event), None);
    }
}

#[derive(Default)]
struct AgentFailureFields {
    provider_disabled: bool,
}

impl Visit for AgentFailureFields {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "provider_disabled" {
            self.provider_disabled = value;
        }
    }

    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

fn report(event: &TracingEvent<'_>) -> Event<'static> {
    let metadata = event.metadata();
    let agent_failure = metadata.target() == "intent_services::agent_terminal_failure";
    let value = agent_failure.then(|| {
        let mut fields = AgentFailureFields::default();
        event.record(&mut fields);
        if fields.provider_disabled {
            "Agent turn could not start because its provider is disabled. Enable the provider in Settings > Providers or choose an enabled provider or model."
        } else {
            "Agent turn failed"
        }
        .to_owned()
    });
    // Keep relative Rust source locations within the checkout. Absolute
    // compiler paths can reveal the build machine's username and directories.
    let source = metadata.file().and_then(|path| {
        let relative = path.strip_prefix("./").unwrap_or(path);
        (relative.starts_with("crates/") && !relative.contains("..")).then_some(relative.to_owned())
    });
    let frame = Frame {
        module: Some(
            if agent_failure {
                metadata.module_path().unwrap_or(metadata.target())
            } else {
                metadata.target()
            }
            .to_owned(),
        ),
        filename: source,
        lineno: metadata.line().map(u64::from),
        in_app: Some(true),
        ..Default::default()
    };
    Event {
        platform: "rust".into(),
        level: Level::Error,
        release: Some(
            format!(
                "intentd@{}+{}",
                env!("CARGO_PKG_VERSION"),
                intent_transport::BUILD_COMMIT.unwrap_or("local")
            )
            .into(),
        ),
        tags: [(
            "component".to_owned(),
            if agent_failure { "agent" } else { "daemon" }.to_owned(),
        )]
        .into(),
        exception: vec![Exception {
            ty: if agent_failure {
                "IntentAgentFailure"
            } else {
                "IntentDaemonError"
            }
            .into(),
            value,
            stacktrace: Some(Stacktrace {
                frames: vec![frame],
                ..Default::default()
            }),
            ..Default::default()
        }]
        .into(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Default)]
    struct TestTransport(Mutex<Vec<u8>>);

    impl sentry::Transport for TestTransport {
        fn send_envelope(&self, envelope: sentry::Envelope) {
            envelope.to_writer(&mut *self.0.lock().unwrap()).unwrap();
        }
    }

    #[test]
    fn reports_only_first_party_errors_without_context() {
        let transport = Arc::new(TestTransport::default());
        let reporter = Reporter(Arc::new(sentry::Client::from(sentry::ClientOptions {
            dsn: Some("https://public@example.invalid/1".parse().unwrap()),
            transport: Some(Arc::new(Arc::clone(&transport))),
            ..Default::default()
        })));
        use tracing_subscriber::Layer as _;
        let subscriber = tracing_subscriber::registry()
            .with(reporter.with_filter(tracing_subscriber::filter::filter_fn(should_report)));
        let report_line = tracing::subscriber::with_default(subscriber, || {
            let _span = tracing::error_span!("private", prompt = "PRIVATE_PROMPT").entered();
            tracing::warn!(target: "intent_fixture", "PRIVATE_WARN");
            tracing::error!(target: "external_library", "PRIVATE_DEPENDENCY_ERROR");
            let report_line = line!() + 1;
            tracing::error!(target: "intent_services::agent_terminal_failure", provider_disabled = true, path = "/private/workspace/repo", "PRIVATE_AGENT_ERROR");
            report_line
        });
        let report = String::from_utf8(transport.0.lock().unwrap().clone()).unwrap();
        assert_eq!(report.matches("IntentAgentFailure").count(), 1);
        assert!(!report.contains("PRIVATE_"));
        assert!(!report.contains("/private/workspace"));
        let event: serde_json::Value =
            serde_json::from_str(report.lines().nth(2).unwrap()).unwrap();
        let exception = &event["exception"]["values"][0];
        assert!(exception["value"]
            .as_str()
            .unwrap()
            .contains("provider is disabled"));
        let frame = &exception["stacktrace"]["frames"][0];
        assert_eq!(frame["lineno"], report_line);
        assert_eq!(frame["filename"], "crates/intentd/src/error_reporting.rs");
        assert_eq!(frame["module"], module_path!());
    }
}
