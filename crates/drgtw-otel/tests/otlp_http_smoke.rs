//! OTLP/HTTP wire smoke test (WP-6).
//!
//! Stands up a minimal OTLP/HTTP receiver via `wiremock` that matches
//! `POST /v1/traces`, builds the gateway's OTLP/HTTP span exporter (exactly as
//! `drgtw_otel::init` does for `protocol = "http"`), exports one allow-listed
//! span, then asserts the collector received >= 1 protobuf request whose
//! decoded payload surfaces our span name and carries NONE of the forbidden
//! attribute keys.
//!
//! The exporter's async `export()` is driven directly on the tokio runtime
//! (the SDK's sync span processors use `futures_executor::block_on`, which
//! cannot drive the async reqwest HTTP client this exporter is built on). The
//! privacy-critical assertion — what reaches the wire — is identical to the
//! production batch path.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use opentelemetry::InstrumentationScope;
use opentelemetry::KeyValue;
use opentelemetry::trace::{
    SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
};
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::trace::{SpanData, SpanEvents, SpanExporter as _, SpanLinks};
use prost::Message;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use drgtw_otel::keys;

use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};

#[derive(Clone)]
struct CaptureResponder {
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Respond for CaptureResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.bodies.lock().unwrap().push(req.body.clone());
        // OTLP/HTTP expects a protobuf ExportTraceServiceResponse body with the
        // application/x-protobuf content type.
        let body = ExportTraceServiceResponse::default().encode_to_vec();
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/x-protobuf")
            .set_body_bytes(body)
    }
}

fn allow_listed_span() -> SpanData {
    let span_context = SpanContext::new(
        TraceId::from_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]),
        SpanId::from_bytes([1, 2, 3, 4, 5, 6, 7, 8]),
        TraceFlags::SAMPLED,
        false,
        TraceState::default(),
    );
    SpanData {
        span_context,
        parent_span_id: SpanId::INVALID,
        parent_span_is_remote: false,
        span_kind: SpanKind::Client,
        name: Cow::Borrowed("chat gpt-4o"),
        start_time: SystemTime::now(),
        end_time: SystemTime::now(),
        attributes: vec![
            KeyValue::new(keys::GEN_AI_OPERATION_NAME, "chat"),
            KeyValue::new(keys::GEN_AI_REQUEST_MODEL, "gpt-4o"),
            KeyValue::new(keys::DRGTW_CONNECTION, "openai"),
        ],
        dropped_attributes_count: 0,
        events: SpanEvents::default(),
        links: SpanLinks::default(),
        status: Status::Ok,
        instrumentation_scope: InstrumentationScope::builder("drgtw").build(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn otlp_http_traces_reach_collector_without_forbidden_keys() {
    let server = MockServer::start().await;
    let bodies = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    Mock::given(method("POST"))
        .and(path("/v1/traces"))
        .respond_with(CaptureResponder {
            bodies: bodies.clone(),
        })
        .mount(&server)
        .await;

    // The OTLP/HTTP builder treats `with_endpoint` as the exact signal URL, so
    // the gateway appends `/v1/traces` to the configured base (see
    // `drgtw_otel::otlp_signal_endpoint`). Exercise that exact helper here.
    let endpoint = drgtw_otel::otlp_signal_endpoint(&server.uri(), "/v1/traces");
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .with_timeout(Duration::from_secs(5))
        .build()
        .expect("build OTLP/HTTP span exporter");

    exporter
        .export(vec![allow_listed_span()])
        .await
        .expect("export ok");

    let captured = bodies.lock().unwrap().clone();
    assert!(
        !captured.is_empty(),
        "collector received no OTLP/HTTP trace request"
    );

    let mut found_span = false;
    let mut all_attr_keys: Vec<String> = Vec::new();
    for body in &captured {
        let req = ExportTraceServiceRequest::decode(body.as_slice())
            .expect("decode ExportTraceServiceRequest");
        for rs in &req.resource_spans {
            for ss in &rs.scope_spans {
                for span in &ss.spans {
                    if span.name == "chat gpt-4o" {
                        found_span = true;
                    }
                    for attr in &span.attributes {
                        all_attr_keys.push(attr.key.clone());
                    }
                }
            }
        }
    }
    assert!(found_span, "decoded OTLP did not surface our span name");
    for forbidden in keys::FORBIDDEN {
        assert!(
            !all_attr_keys.iter().any(|k| k == forbidden),
            "forbidden key `{forbidden}` present on the wire"
        );
    }
    // Sanity: only allow-listed keys reached the wire.
    for k in &all_attr_keys {
        assert!(
            keys::ALLOW_LIST.contains(&k.as_str()),
            "non-allow-listed key `{k}` reached the wire"
        );
    }
}
