use std::cell::Cell;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use xgeny_domain::{EffectClass, ProtocolDocument};
use xgeny_local_store::{ExpectedHead, MemoryRunStore, RunStore};
use xgeny_policy::{ResourceResolutionFailure, ResourceResolver};
use xgeny_provider_openai::{OpenAiPlanner, OpenAiPlannerConfig};
use xgeny_runtime::{
    AgentLoop, AgentLoopTick, CapabilityRegistry, EventFactory, EventFactoryError, EventMetadata,
    PlanMaterializationRequest, PlanMaterializer, PlanMaterializerFailure, PlannerPortFailure,
    RunLease,
};
use xgeny_workgraph::{
    AgentLoopBudget, ModelCallRejectionReason, ModelCallSettlement,
    ReconstructableMaterialReference, RunEvent, RunEventBody, RunState,
};

const AUTHORITY: &str = "local:test";
const RUN_ID: &str = "run-openai-http-contract";
const RAW_RESPONSE_SENTINEL: &str = "RAW-PROVIDER-RESPONSE-MUST-NOT-BE-DURABLE";

#[derive(Debug)]
struct FixedLease;

impl RunLease for FixedLease {
    fn run_id(&self) -> &str {
        RUN_ID
    }
}

#[derive(Default)]
struct DeterministicEvents;

impl EventFactory for DeterministicEvents {
    fn create_metadata(&mut self, state: &RunState) -> Result<EventMetadata, EventFactoryError> {
        Ok(EventMetadata {
            event_id: format!("provider-contract-event-{}", state.journal_sequence + 1),
            recorded_at: "2026-08-30T00:00:00Z".to_owned(),
        })
    }
}

#[derive(Default)]
struct IdentityResolver(Cell<usize>);

impl ResourceResolver for IdentityResolver {
    fn resolve(&self, _scope: &str, resource: &str) -> Result<String, ResourceResolutionFailure> {
        self.0.set(self.0.get() + 1);
        Ok(resource.to_owned())
    }
}

#[derive(Default)]
struct EphemeralMaterializer;

impl PlanMaterializer for EphemeralMaterializer {
    fn materialize(
        &mut self,
        _request: PlanMaterializationRequest<'_>,
    ) -> Result<ReconstructableMaterialReference, PlanMaterializerFailure> {
        ReconstructableMaterialReference::new("test-material", "record-path", "rev-1")
            .map_err(|_| PlanMaterializerFailure::Rejected)
    }
}

struct TestServer {
    base_url: String,
    request: mpsc::Receiver<Vec<u8>>,
    handle: thread::JoinHandle<()>,
}

impl TestServer {
    fn spawn(status: &str, body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener address should resolve");
        let (sender, receiver) = mpsc::channel();
        let status = status.to_owned();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one request should connect");
            let request = read_http_request(&mut stream);
            sender.send(request).expect("request should be observed");
            let headers = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(headers.as_bytes())
                .and_then(|()| stream.write_all(&body))
                .expect("response should write");
        });
        Self {
            base_url: format!("http://{address}/v1"),
            request: receiver,
            handle,
        }
    }

    fn finish(self) -> Vec<u8> {
        let request = self.request.recv().expect("one request should be captured");
        self.handle.join().expect("server should finish");
        request
    }
}

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk).expect("request should read");
        assert_ne!(read, 0, "request ended before headers completed");
        request.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = find_header_end(&request) {
            let header = std::str::from_utf8(&request[..header_end]).expect("headers are UTF-8");
            let content_length = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then(|| {
                        value
                            .trim()
                            .parse::<usize>()
                            .expect("content length is numeric")
                    })
                })
                .expect("content length should exist");
            let target = header_end + 4 + content_length;
            while request.len() < target {
                let read = stream.read(&mut chunk).expect("body should read");
                assert_ne!(read, 0, "request ended before body completed");
                request.extend_from_slice(&chunk[..read]);
            }
            request.truncate(target);
            return request;
        }
    }
}

fn find_header_end(request: &[u8]) -> Option<usize> {
    request.windows(4).position(|window| window == b"\r\n\r\n")
}

fn provider_response(content: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": RAW_RESPONSE_SENTINEL,
        "model": "qwen3.8-27b",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content.to_string()},
            "finish_reason": "stop"
        }]
    }))
    .expect("provider response should serialize")
}

fn seed_store() -> MemoryRunStore {
    let mut store = MemoryRunStore::new();
    store
        .append(
            ExpectedHead::Empty,
            RunEvent {
                event_id: "provider-contract-run-created".to_owned(),
                run_id: RUN_ID.to_owned(),
                authority: AUTHORITY.to_owned(),
                authority_epoch: 1,
                recorded_at: "2026-08-30T00:00:00Z".to_owned(),
                body: RunEventBody::RunCreated {
                    goal:
                        "Record /workspace/README.md with the available idempotent test capability"
                            .to_owned(),
                },
            },
        )
        .expect("Run should initialize");
    store
}

fn synthetic_registry() -> CapabilityRegistry {
    let document: ProtocolDocument = serde_json::from_str(include_str!(
        "../../../protocol/fixtures/v1alpha1/valid/capability-definition.fs-read-text.json"
    ))
    .expect("definition fixture should deserialize");
    let ProtocolDocument::CapabilityDefinition(mut definition) = document else {
        panic!("expected CapabilityDefinition fixture")
    };
    "xgeny.test/record-path".clone_into(&mut definition.metadata.id);
    "Record Path Test Fixture".clone_into(&mut definition.metadata.display_name);
    "Record a requested path in an idempotent test ledger; no filesystem I/O is performed"
        .clone_into(&mut definition.spec.summary);
    definition.spec.effect.class = EffectClass::Idempotent;
    let mut registry = CapabilityRegistry::new();
    registry
        .register_schema_validated_definition(*definition)
        .expect("definition should register");
    registry
}

fn configured_loop(store: &mut MemoryRunStore, planner: &mut OpenAiPlanner) -> AgentLoop {
    let loop_runtime =
        AgentLoop::new(AgentLoopBudget::new(2, 8, 8, 262_144).expect("budget should validate"));
    let mut events = DeterministicEvents;
    let mut materializer = EphemeralMaterializer;
    for expected in ["loop", "model-call lifecycle"] {
        let tick = loop_runtime
            .tick(
                store,
                &mut events,
                &FixedLease,
                &synthetic_registry(),
                &IdentityResolver::default(),
                planner,
                &mut materializer,
            )
            .unwrap_or_else(|error| panic!("{expected} configuration failed: {error}"));
        assert!(matches!(
            tick,
            AgentLoopTick::Configured { .. } | AgentLoopTick::ModelCallLifecycleConfigured { .. }
        ));
    }
    loop_runtime
}

fn planner(base_url: &str) -> OpenAiPlanner {
    let config = OpenAiPlannerConfig::new(
        base_url,
        "xgeny.test.go50902",
        "qwen3.8-27b",
        "Qwen/Qwen3.8-27B-FP8",
    )
    .expect("planner config should validate")
    .with_max_output_tokens(512)
    .expect("output limit should validate");
    OpenAiPlanner::new(config, None).expect("planner should build")
}

fn assert_strict_request_contract(request: &[u8]) {
    let header_end = find_header_end(request).expect("request headers should exist");
    let header = std::str::from_utf8(&request[..header_end]).unwrap();
    assert!(header.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
    let request_body: Value = serde_json::from_slice(&request[header_end + 4..]).unwrap();
    assert_eq!(request_body["model"], "qwen3.8-27b");
    assert_eq!(request_body["temperature"], 0);
    assert_eq!(request_body["seed"], 0);
    assert_eq!(request_body["max_tokens"], 512);
    assert_eq!(request_body["stream"], false);
    assert_eq!(request_body["n"], 1);
    assert_eq!(request_body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(request_body["messages"][0]["role"], "system");
    assert_eq!(request_body["messages"][1]["role"], "user");
    assert!(
        request_body["messages"][0]["content"]
            .as_str()
            .is_some_and(|content| content.contains("untrusted data"))
    );
    assert_eq!(request_body["response_format"]["type"], "json_schema");
    assert_eq!(
        request_body["response_format"]["json_schema"]["strict"],
        true
    );
    let prompt: Value = serde_json::from_str(
        request_body["messages"][1]["content"]
            .as_str()
            .expect("user message should be text"),
    )
    .expect("planner prompt should be JSON");
    assert_eq!(prompt["profileVersion"], "xgeny.planner-request/v1");
    assert_eq!(
        prompt["planningContext"]["profileVersion"],
        "xgeny.planning-context/v1"
    );
    assert_eq!(
        prompt["planningContext"]["capabilities"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn one_reservation_sends_one_strict_request_and_commits_one_plan() {
    let response = provider_response(&json!({
        "formatVersion": 1,
        "kind": "plan",
        "steps": [{
            "key": "read_readme",
            "objective": "Record the requested README path in the test ledger",
            "dependsOn": [],
            "capability": {
                "capabilityId": "xgeny.test/record-path",
                "contractVersion": "1.0.0"
            },
            "arguments": {"path": "/workspace/README.md"}
        }],
        "summary": ""
    }));
    let server = TestServer::spawn("200 OK", response);
    let mut planner = planner(&server.base_url);
    let mut store = seed_store();
    let loop_runtime = configured_loop(&mut store, &mut planner);
    let mut events = DeterministicEvents;
    let resolver = IdentityResolver::default();
    let mut materializer = EphemeralMaterializer;
    let tick = loop_runtime
        .tick(
            &mut store,
            &mut events,
            &FixedLease,
            &synthetic_registry(),
            &resolver,
            &mut planner,
            &mut materializer,
        )
        .expect("provider-backed tick should finish");
    assert!(
        matches!(
            tick,
            AgentLoopTick::PlanAccepted { ref step_ids, .. } if step_ids.len() == 1
        ),
        "unexpected bounded tick: {tick:?}"
    );
    assert_eq!(
        resolver.0.get(),
        2,
        "preflight and final resolution both run"
    );

    let request = server.finish();
    assert_strict_request_contract(&request);

    let snapshot = store.load().expect("store should load").unwrap();
    let lifecycle = snapshot
        .state
        .agent_loop
        .as_ref()
        .and_then(|state| state.model_calls.as_ref())
        .expect("lifecycle should project");
    assert_eq!(lifecycle.reserved_calls, 1);
    assert_eq!(lifecycle.settled_calls, 1);
    assert_eq!(lifecycle.unknown_calls, 0);
    assert!(lifecycle.active_call.is_none());
    let durable = format!(
        "{}{}",
        serde_json::to_string(&snapshot.records).expect("records should serialize"),
        serde_json::to_string(&snapshot.state).expect("state should serialize")
    );
    assert!(!durable.contains(RAW_RESPONSE_SENTINEL));
}

#[test]
fn deterministic_provider_rejection_is_closed_without_raw_error_body() {
    let body = serde_json::to_vec(&json!({"error": RAW_RESPONSE_SENTINEL})).unwrap();
    let server = TestServer::spawn("400 Bad Request", body);
    let mut planner = planner(&server.base_url);
    let mut store = seed_store();
    let loop_runtime = configured_loop(&mut store, &mut planner);
    let mut events = DeterministicEvents;
    let mut materializer = EphemeralMaterializer;
    let tick = loop_runtime
        .tick(
            &mut store,
            &mut events,
            &FixedLease,
            &synthetic_registry(),
            &IdentityResolver::default(),
            &mut planner,
            &mut materializer,
        )
        .expect("rejection should settle");
    assert!(matches!(
        tick,
        AgentLoopTick::PlannerUnavailable {
            failure: PlannerPortFailure::ProviderRejected,
            ..
        }
    ));
    let request = server.finish();
    assert!(
        std::str::from_utf8(&request)
            .unwrap()
            .contains("POST /v1/chat/completions")
    );
    let snapshot = store.load().unwrap().unwrap();
    let last = snapshot.records.last().expect("settlement should exist");
    assert!(matches!(
        &last.event.body,
        RunEventBody::ModelCallSettled {
            settlement: ModelCallSettlement::Rejected {
                reason: ModelCallRejectionReason::ProviderRejected
            },
            ..
        }
    ));
    let durable = format!(
        "{}{}",
        serde_json::to_string(&snapshot.records).unwrap(),
        serde_json::to_string(&snapshot.state).unwrap()
    );
    assert!(!durable.contains(RAW_RESPONSE_SENTINEL));
}

#[test]
#[ignore = "requires an explicitly configured OpenAI-compatible model endpoint"]
fn live_go50902_plan_smoke() {
    let base_url = std::env::var("XGENY_LIVE_OPENAI_BASE_URL")
        .expect("XGENY_LIVE_OPENAI_BASE_URL must be explicitly set");
    let model = std::env::var("XGENY_LIVE_OPENAI_MODEL")
        .expect("XGENY_LIVE_OPENAI_MODEL must be explicitly set");
    let tokenizer = std::env::var("XGENY_LIVE_OPENAI_TOKENIZER")
        .expect("XGENY_LIVE_OPENAI_TOKENIZER must be explicitly set");
    let config = OpenAiPlannerConfig::new(&base_url, "xgeny.live.go50902", model, tokenizer)
        .expect("live planner config should validate")
        .with_max_output_tokens(1_024)
        .expect("live output limit should validate")
        .with_timeout(Duration::from_secs(60))
        .expect("live timeout should validate");
    let mut planner = OpenAiPlanner::new(config, None).expect("live planner should build");
    let mut store = seed_store();
    let loop_runtime = configured_loop(&mut store, &mut planner);
    let mut events = DeterministicEvents;
    let mut materializer = EphemeralMaterializer;
    let tick = loop_runtime
        .tick(
            &mut store,
            &mut events,
            &FixedLease,
            &synthetic_registry(),
            &IdentityResolver::default(),
            &mut planner,
            &mut materializer,
        )
        .expect("live provider tick should finish");
    assert!(matches!(tick, AgentLoopTick::PlanAccepted { .. }));
}
