//! Telemetry initialization.
//!
//! Controlled by `OTEL_EXPORTER_OTLP_ENDPOINT` (the standard OTLP env var):
//! - unset → no-op (tracing disabled, zero overhead)
//! - `"stderr"` → JSON spans/events to stderr (non-standard extension)
//! - `"http://..."` → OTLP HTTP export (traces + logs) to the given endpoint
//!
//! ## Distributed tracing
//!
//! If `TRACEPARENT` is set (W3C Trace Context format), spans are created as
//! children of the remote parent. Use [`current_traceparent`] to extract the
//! current span context for propagation to child processes.

use tracing_subscriber::EnvFilter;

/// Opaque guard — dropping it flushes and shuts down the OTLP pipeline.
/// Hold this in `main()` until exit.
pub struct TelemetryGuard {
    #[cfg(feature = "otel")]
    trace_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    #[cfg(feature = "otel")]
    log_provider: Option<opentelemetry_sdk::logs::SdkLoggerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        #[cfg(feature = "otel")]
        {
            if let Some(provider) = self.trace_provider.take()
                && let Err(e) = provider.shutdown()
            {
                eprintln!("otel trace shutdown error: {e}");
            }
            if let Some(provider) = self.log_provider.take()
                && let Err(e) = provider.shutdown()
            {
                eprintln!("otel log shutdown error: {e}");
            }
        }
    }
}

/// Initialize telemetry based on `OTEL_EXPORTER_OTLP_ENDPOINT`.
///
/// Returns a guard that must be held until the program exits.
/// Dropping the guard flushes any pending spans and logs.
#[must_use]
pub fn init() -> TelemetryGuard {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();

    match endpoint.as_deref() {
        None | Some("") => init_noop(),
        Some("stderr") => init_stderr(),
        #[cfg(feature = "otel")]
        Some(_) => init_otlp(),
        #[cfg(not(feature = "otel"))]
        Some(_) => {
            eprintln!(
                "warning: OTEL_EXPORTER_OTLP_ENDPOINT set but bones built without 'otel' feature"
            );
            init_noop()
        }
    }
}

fn init_noop() -> TelemetryGuard {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let filter = EnvFilter::try_from_env("BONES_LOG").unwrap_or_else(|_| {
        let debug = std::env::var("DEBUG").is_ok();
        EnvFilter::new(if debug {
            "bones=debug,info"
        } else {
            "bones=info,warn"
        })
    });

    let format = std::env::var("BONES_LOG_FORMAT").unwrap_or_else(|_| "compact".to_string());

    let _ = match format.as_str() {
        "json" => tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().json().with_ansi(false))
            .try_init(),
        _ => tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().compact())
            .try_init(),
    };

    TelemetryGuard {
        #[cfg(feature = "otel")]
        trace_provider: None,
        #[cfg(feature = "otel")]
        log_provider: None,
    }
}

/// JSON spans/events to stderr via tracing-subscriber's JSON formatter.
fn init_stderr() -> TelemetryGuard {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::io::stderr)
                .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE),
        )
        .init();

    TelemetryGuard {
        #[cfg(feature = "otel")]
        trace_provider: None,
        #[cfg(feature = "otel")]
        log_provider: None,
    }
}

/// OTLP HTTP export (traces + logs).
///
/// The SDK reads `OTEL_EXPORTER_OTLP_ENDPOINT` from the environment natively
/// and appends `/v1/traces` or `/v1/logs` as appropriate.
#[cfg(feature = "otel")]
fn init_otlp() -> TelemetryGuard {
    use opentelemetry::trace::TracerProvider as _;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    // --- Traces ---
    let span_exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("warning: failed to init OTLP span exporter: {e}");
            return init_noop();
        }
    };

    let resource = otel_resource();

    let trace_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_simple_exporter(span_exporter)
        .with_resource(resource.clone())
        .build();

    let tracer = trace_provider.tracer(env!("CARGO_PKG_NAME"));
    let trace_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    // --- Logs ---
    let log_exporter = match opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("warning: failed to init OTLP log exporter: {e}");
            return init_noop();
        }
    };

    let log_provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
        .with_simple_exporter(log_exporter)
        .with_resource(resource)
        .build();

    let log_layer =
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&log_provider);

    // --- Parent context (distributed tracing) ---
    install_parent_context();

    // --- Subscriber ---
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(trace_layer)
        .with(log_layer)
        .init();

    TelemetryGuard {
        trace_provider: Some(trace_provider),
        log_provider: Some(log_provider),
    }
}

/// Extract the current span's trace context as a W3C `TRACEPARENT` string.
///
/// Returns `None` if OTEL is not enabled or no valid span context exists.
/// Use this to propagate trace context to child processes via environment
/// variables.
#[cfg(feature = "otel")]
#[allow(dead_code)]
pub fn current_traceparent() -> Option<String> {
    use opentelemetry::propagation::TextMapPropagator as _;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use std::collections::HashMap;

    let propagator = TraceContextPropagator::new();
    let mut carrier: HashMap<String, String> = HashMap::new();
    propagator.inject(&mut carrier);
    carrier.remove("traceparent")
}

/// Stub when otel feature is disabled.
#[cfg(not(feature = "otel"))]
#[allow(dead_code)]
pub fn current_traceparent() -> Option<String> {
    None
}

/// If `TRACEPARENT` is set, parse it and install as the current `OTel` context
/// so that subsequent spans become children of the remote parent.
#[cfg(feature = "otel")]
fn install_parent_context() {
    use opentelemetry::propagation::TextMapPropagator as _;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use std::collections::HashMap;

    if let Ok(traceparent) = std::env::var("TRACEPARENT") {
        let mut carrier: HashMap<String, String> = HashMap::new();
        carrier.insert("traceparent".to_string(), traceparent);
        let propagator = TraceContextPropagator::new();
        let cx = propagator.extract(&carrier);
        // Attach as the current context — tracing-opentelemetry's layer will
        // pick this up as the parent for root-level spans.
        let _guard = cx.attach();
        // The guard is intentionally leaked: we want this context to remain
        // active for the lifetime of the process.
        std::mem::forget(_guard);
    }
}

#[cfg(feature = "otel")]
fn otel_resource() -> opentelemetry_sdk::Resource {
    use opentelemetry::KeyValue;
    opentelemetry_sdk::Resource::builder()
        .with_attribute(KeyValue::new("service.name", env!("CARGO_PKG_NAME")))
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .build()
}
