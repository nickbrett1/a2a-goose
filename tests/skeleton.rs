//! M1's done-when, end to end and in-process: the card is served, `ask` plus
//! mined and declared skills are on it, `POST /` needs a token, and an A2A
//! `SendMessage` round-trips through the stub executor.
//!
//! The client is the SDK's own (`a2a-client-lf`), not a hand-written HTTP call
//! (constraint #17). That matters more than it looks: the assertions are made
//! against *the SDK's* types, so a wire change is caught by the SDK's serde
//! rather than by string matching against whatever this repo happened to assume.
//! Raw HTTP is used only where the SDK has no opinion — an unauthenticated POST
//! has no SDK representation, because a client that always authenticates is the
//! point of an auth interceptor.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use a2a::{AgentCard, Message, Part, Role, SendMessageRequest, SendMessageResponse, TaskState};
use a2a_client::A2AClientFactory;
use a2a_client::auth::AuthInterceptor;
use a2a_client::client::SendMessageExt;
use a2a_goose::activity::ActivityHub;
use a2a_goose::card;
use a2a_goose::config::Config;
use a2a_goose::goose::{Goose, Version};
use a2a_goose::registry::Registry;
use a2a_goose::server::{self, Agent};
use a2a_goose::skills::SkillSet;
use a2a_goose::turn::{SessionClose, SessionInfo, TurnError, TurnEvent, TurnRequest, Turns, Usage};
use futures::StreamExt;
use futures::future::BoxFuture;
use serde_json::Value;

const TOKEN: &str = "skeleton-test-token";

/// What the fake turn says.
const ANSWER: &str = "hello from the fake turn";

/// A [`Turns`] with no ACP behind it.
///
/// This file pins the *A2A wire* — the card, the auth layer, the JSON-RPC method
/// names, the shape of a completed task — and none of that should need a
/// `goose serve` running in CI. The ACP hop has its own tests, against a
/// recording and against a real goose.
#[derive(Default)]
struct FakeTurns {
    /// What each turn was asked to do, so a test can assert the *prompt* the
    /// executor built and not just the answer it produced.
    requests: std::sync::Mutex<Vec<TurnRequest>>,
    /// What `GET /sessions` reports. Empty unless a test puts something there,
    /// which is the honest default for a runner that holds no sessions.
    held: std::sync::Mutex<Vec<SessionInfo>>,
    /// What `DELETE /sessions/{contextId}` finds. `Absent` by default, for the
    /// same reason.
    closing: std::sync::Mutex<SessionClose>,
}

impl Turns for FakeTurns {
    fn run(
        &self,
        request: TurnRequest,
    ) -> futures::stream::BoxStream<'static, Result<TurnEvent, TurnError>> {
        self.requests.lock().expect("lock").push(request);
        Box::pin(futures::stream::iter(vec![
            Ok(TurnEvent::Text(ANSWER.to_string())),
            Ok(TurnEvent::Finished {
                stop_reason: "end_turn".to_string(),
                usage: Usage {
                    total: 7,
                    input: 5,
                    output: 2,
                },
            }),
        ]))
    }

    fn sessions(&self) -> Vec<SessionInfo> {
        self.held.lock().expect("lock").clone()
    }

    /// Kept in step with [`Self::sessions`] on purpose. The real runner reads
    /// both from one pool, so the two answers agree there by construction; a
    /// fake that disagreed with itself would let a `sessions` envelope pass a
    /// test that no real host could run.
    fn retained(&self) -> usize {
        self.held.lock().expect("lock").len()
    }

    /// The outcome is scripted rather than derived from the context: this file
    /// pins the *wire*, and the part of the close that is this repo's code is
    /// the mapping from an outcome to a status code, not the lookup. The lookup
    /// is proved against a fake `goose serve`, where a session is a thing that
    /// can actually be closed (`tests/session_reuse.rs`).
    fn close_session(&self, _context: String) -> BoxFuture<'static, SessionClose> {
        let outcome = self.closing.lock().expect("lock").clone();
        Box::pin(async move { outcome })
    }
}

/// A booted server on an ephemeral port, plus the directories behind its card.
struct Fixture {
    base: String,
    recipes: PathBuf,
    skills_d: PathBuf,
    turns: Arc<FakeTurns>,
}

impl Fixture {
    /// The turns this server has been asked to run.
    fn ran(&self) -> Vec<TurnRequest> {
        self.turns.requests.lock().expect("lock").clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.recipes);
        let _ = std::fs::remove_dir_all(&self.skills_d);
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "a2a-goose-skeleton-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("temp dir");
    path
}

async fn boot() -> Fixture {
    let recipes = temp_dir("recipes");
    let skills_d = temp_dir("skillsd");

    // One mined recipe and one declared skill, so the served card proves the
    // whole merge rather than just `ask`.
    std::fs::write(
        recipes.join("scaffold-project.yaml"),
        "version: 1\ntitle: Scaffold a project\ndescription: Lay out a new repo\n\
         prompt: lay out the repo\ninstructions: the actual secret sauce\n",
    )
    .expect("write recipe");
    std::fs::write(
        skills_d.join("code-review.yaml"),
        "id: code-review\nname: Code review\ndescription: Review a diff\ntags: [review]\n\
         instruction: |\n  Review the current diff.\n",
    )
    .expect("write skill");

    let mut config = Config::default();
    // The advertised address is deliberately *not* loopback: it is what the card
    // carries, and the test dials the real listener address separately.
    config.server.public_url = "http://mac-studio.tail86fd19.ts.net:10001".to_string();
    config.goose.defaults.allowed_roots = vec![PathBuf::from("/tmp")];
    config.goose.defaults.cwd = PathBuf::from("/tmp");
    config.skills.recipes.search_paths = vec![recipes.clone()];
    config.skills.recipes.enabled = vec!["scaffold-project".to_string()];
    config.skills.d = skills_d.clone();
    // A name nothing in the environment can be using, so registration is
    // `unconfigured` and the test never touches a proxy.
    config.registry.master_key_env = "A2A_GOOSE_SKELETON_KEY_UNSET_7c1a".to_string();
    config
        .validate()
        .expect("the fixture config must be a valid one");

    let skills = SkillSet::load(&config.skills).expect("skills");
    let card = card::assemble(&config, &skills);
    let card_hash = card::hash(&card);
    let turns = Arc::new(FakeTurns::default());
    // Enabled, so a test can watch it; the hub is shared by the `Agent` and the
    // executor the router builds from it.
    let activity = Arc::new(ActivityHub::default());

    // From *this* config, so the registry reads the unset `master_key_env` the
    // fixture set above and is `unconfigured` here — not `LITELLM_MASTER_KEY`,
    // which a dev container exports, making `/status` report `unregistered` and
    // this test fail on a developer's machine while passing in CI.
    let registry = Registry::new(&config);

    let agent = Arc::new(Agent {
        config,
        skills: Arc::new(skills),
        card,
        card_hash,
        goose: Goose {
            path: "/usr/local/bin/goose".into(),
            version: Version::new(1, 50, 0),
        },
        registry,
        turns: turns.clone(),
        serve: None,
        activity: Arc::clone(&activity),
        started: Instant::now(),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let token: Arc<str> = Arc::from(TOKEN);
    tokio::spawn(async move {
        server::serve(listener, agent, token).await.expect("serve");
    });

    Fixture {
        base: format!("http://{addr}"),
        recipes,
        skills_d,
        turns,
    }
}

fn rpc_body(text: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "SendMessage",
        "params": {
            "message": {
                "messageId": "m1",
                "role": "ROLE_USER",
                "parts": [{ "text": text }],
            }
        }
    })
}

#[tokio::test]
async fn the_jsonrpc_wire_is_pinned_independently_of_the_sdk() {
    // Every other test in this file reads the wire through `a2a-client-lf`, which
    // is the same SDK that wrote it — so a serialisation change would move both
    // sides together and pass. This one reads it raw, and pins the three things a
    // caller outside this repo would notice: the method names, the state
    // vocabulary, and the fact that a response is the POST body.
    let fixture = boot().await;
    let response: Value = reqwest::Client::new()
        .post(format!("{}/", fixture.base))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "SendMessage",
            "params": {
                "message": {
                    "messageId": "m1",
                    "role": "ROLE_USER",
                    "parts": [{ "text": "hello" }],
                }
            }
        }))
        .send()
        .await
        .expect("POST")
        .json()
        .await
        .expect("a unary call answers in the POST body");

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 7);
    let task = &response["result"]["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
    // The answer is an artifact, not a status message: it is what a caller
    // reads, and it is the same thing a streaming caller watches arrive.
    assert_eq!(task["artifacts"][0]["artifactId"], "answer");
    assert_eq!(task["artifacts"][0]["parts"][0]["text"], ANSWER);
    assert!(task["id"].is_string());
    assert!(task["contextId"].is_string());

    // And the streaming method is the SDK's own name for it, not `message/stream`.
    let stream = reqwest::Client::new()
        .post(format!("{}/", fixture.base))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "SendStreamingMessage",
            "params": {
                "message": {
                    "messageId": "m2",
                    "role": "ROLE_USER",
                    "parts": [{ "text": "hello again" }],
                }
            }
        }))
        .send()
        .await
        .expect("POST stream")
        .text()
        .await
        .expect("SSE body");
    assert!(
        stream.contains("data: "),
        "SSE frames are `data:` lines: {stream}"
    );
    assert!(
        stream.contains("\"TASK_STATE_COMPLETED\""),
        "the streamed task is the same shape: {stream}"
    );
}

/// The served card, as the SDK parses it, with the interface address rewritten to
/// the port this test is actually listening on.
///
/// The rewrite is the caller's half of the S9 problem made explicit: the card
/// *advertises* the tailnet name, and a caller that is not on that name has to
/// reach the agent some other way. Dialling what the card says would go to the
/// real host, which is not what a test wants.
async fn dial_card(base: &str) -> AgentCard {
    let mut card = fetch_card(base).await;
    // One interface, by construction (card.rs builds exactly one).
    if let Some(interface) = card.supported_interfaces.first_mut() {
        interface.url = base.to_string();
    }
    card
}

/// The served card, as the SDK parses it — and rewritable to the test's own
/// listening address, which is what a caller on the same host does.
async fn fetch_card(base: &str) -> AgentCard {
    let value: Value = reqwest::get(format!("{base}/.well-known/agent-card.json"))
        .await
        .expect("GET card")
        .json()
        .await
        .expect("card is JSON");
    serde_json::from_value(value).expect("the SDK can read our own card")
}

#[tokio::test]
async fn the_card_is_public_and_carries_ask_plus_every_enabled_skill() {
    let fixture = boot().await;
    let response = reqwest::get(format!("{}/.well-known/agent-card.json", fixture.base))
        .await
        .expect("GET card");
    assert_eq!(response.status(), 200, "discovery needs no token");

    let card = fetch_card(&fixture.base).await;
    let ids: Vec<&str> = card.skills.iter().map(|skill| skill.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["ask", "code-review", "scaffold-project"],
        "ask is implicit, the declared skill comes from skills.d/, the mined one from the recipe"
    );

    let interface = &card.supported_interfaces[0];
    assert_eq!(
        interface.url, "http://mac-studio.tail86fd19.ts.net:10001",
        "the card carries the address LiteLLM dials, not the test's port"
    );
    assert_eq!(interface.protocol_version, "1.0");
    assert_eq!(card.capabilities.streaming, Some(true));

    // The mined skill takes its id from the filename and its name from the
    // recipe's title (S10); its instructions are nowhere near the card.
    let mined = card
        .skills
        .iter()
        .find(|skill| skill.id == "scaffold-project")
        .expect("mined skill");
    assert_eq!(mined.name, "Scaffold a project");
    let rendered = serde_json::to_string(&card).expect("render");
    assert!(
        !rendered.contains("the actual secret sauce"),
        "a recipe's instructions must never reach the card (constraint #12)"
    );
}

#[tokio::test]
async fn posting_without_a_token_is_a_401_and_healthz_is_not_affected() {
    let fixture = boot().await;
    let client = reqwest::Client::new();

    let missing = client
        .post(format!("{}/", fixture.base))
        .json(&rpc_body("hello"))
        .send()
        .await
        .expect("POST without a token");
    assert_eq!(missing.status(), 401);
    assert_eq!(
        missing
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer")
    );

    let wrong = client
        .post(format!("{}/", fixture.base))
        .bearer_auth("not-the-token")
        .json(&rpc_body("hello"))
        .send()
        .await
        .expect("POST with the wrong token");
    assert_eq!(wrong.status(), 401);

    // A token that is a prefix of the real one must not pass either.
    let prefix = client
        .post(format!("{}/", fixture.base))
        .bearer_auth(&TOKEN[..TOKEN.len() - 1])
        .json(&rpc_body("hello"))
        .send()
        .await
        .expect("POST with a prefix token");
    assert_eq!(prefix.status(), 401);

    let health = client
        .get(format!("{}/healthz", fixture.base))
        .send()
        .await
        .expect("GET healthz");
    assert_eq!(health.status(), 200);
    assert_eq!(health.text().await.expect("body"), "ok");
}

#[tokio::test]
async fn an_authenticated_send_round_trips_through_the_executor() {
    let fixture = boot().await;
    let card = dial_card(&fixture.base).await;

    let factory = A2AClientFactory::builder()
        .with_interceptor(Arc::new(AuthInterceptor::bearer(TOKEN)))
        .build();
    let client = factory.create_from_card(&card).await.expect("client");

    let response = client.send_text("hello there").await.expect("send");

    let SendMessageResponse::Task(task) = response else {
        panic!("the executor answers with a task, got {response:?}");
    };
    assert_eq!(task.status.state, TaskState::Completed);
    // The SDK's task snapshot pushes one artifact per artifact *update*, so the
    // answer is the chunks in order rather than one merged part. A unary caller
    // concatenates; a streaming caller gets `append: true` frames instead.
    let answer: String = task
        .artifacts
        .unwrap_or_default()
        .iter()
        .flat_map(|artifact| artifact.parts.iter())
        .filter_map(|part| part.as_text().map(str::to_string))
        .collect();
    assert_eq!(answer, ANSWER);

    // And the turn was asked to run the caller's own text, in the default
    // directory, under the default skill.
    let ran = fixture.ran();
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].prompt, "hello there");
}

#[tokio::test]
async fn an_unknown_skill_id_is_refused_rather_than_falling_back_to_ask() {
    let fixture = boot().await;
    let card = dial_card(&fixture.base).await;

    let factory = A2AClientFactory::builder()
        .with_interceptor(Arc::new(AuthInterceptor::bearer(TOKEN)))
        .build();
    let client = factory.create_from_card(&card).await.expect("client");

    let request = SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text("hello")]),
        configuration: None,
        metadata: Some(
            [("skillId".to_string(), serde_json::json!("no-such-skill"))]
                .into_iter()
                .collect(),
        ),
        tenant: None,
    };

    let err = client
        .send_message(&request)
        .await
        .expect_err("an unknown skillId must not resolve");
    assert_eq!(err.code, a2a::error_code::INVALID_PARAMS);
    assert!(err.message.contains("no-such-skill"), "{}", err.message);
    assert!(
        fixture.ran().is_empty(),
        "a refused skill must not cost a goose session"
    );
}

#[tokio::test]
async fn a_named_skill_sends_its_instruction_and_a_cwd_inside_the_roots() {
    let fixture = boot().await;
    let card = dial_card(&fixture.base).await;

    let factory = A2AClientFactory::builder()
        .with_interceptor(Arc::new(AuthInterceptor::bearer(TOKEN)))
        .build();
    let client = factory.create_from_card(&card).await.expect("client");

    let request = SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text("review this")]),
        configuration: None,
        metadata: Some(
            [
                ("skillId".to_string(), serde_json::json!("code-review")),
                ("cwd".to_string(), serde_json::json!("/tmp")),
            ]
            .into_iter()
            .collect(),
        ),
        tenant: None,
    };

    let SendMessageResponse::Task(task) = client.send_message(&request).await.expect("send") else {
        panic!("expected a task");
    };
    assert_eq!(task.status.state, TaskState::Completed);

    let ran = fixture.ran();
    assert_eq!(ran.len(), 1);
    assert_eq!(
        ran[0].prompt, "Review the current diff.\n\nreview this",
        "the declared skill's instruction is the preamble to the caller's text"
    );
    assert_eq!(
        ran[0].cwd,
        PathBuf::from("/tmp")
            .canonicalize()
            .expect("canonical /tmp"),
        "the caller's cwd is used, canonicalised before the prefix check"
    );
}

#[tokio::test]
async fn status_reports_the_card_the_registry_and_the_goose_it_verified() {
    let fixture = boot().await;
    let payload: Value = reqwest::get(format!("{}/status", fixture.base))
        .await
        .expect("GET status")
        .json()
        .await
        .expect("status is JSON");

    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["goose"]["version"], "1.50.0");
    assert_eq!(payload["registry"]["state"], "unconfigured");
    assert_eq!(payload["card"]["protocolVersion"], "1.0");
    assert_eq!(payload["card"]["hash"].as_str().map(str::len), Some(64));

    let ids: Vec<&str> = payload["skills"]
        .as_array()
        .expect("skills")
        .iter()
        .map(|skill| skill["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, vec!["ask", "code-review", "scaffold-project"]);
    assert_eq!(payload["skills"][0]["default"], true);
}

#[tokio::test]
async fn the_session_control_surface_is_not_open_to_anyone_who_asks() {
    // The bearer layer covers these routes too, and the reason is not symmetry:
    // `/sessions` names the directories other callers are working in, and the
    // `DELETE` ends a conversation's memory. `/status` is open because it
    // carries no secret — these two are a different question.
    let fixture = boot().await;
    let client = reqwest::Client::new();

    let listed = client
        .get(format!("{}/sessions", fixture.base))
        .send()
        .await
        .expect("GET /sessions without a token");
    let deleted = client
        .delete(format!("{}/sessions/c1", fixture.base))
        .send()
        .await
        .expect("DELETE /sessions/{id} without a token");

    for (what, response) in [("GET /sessions", listed), ("DELETE", deleted)] {
        assert_eq!(response.status(), 401, "{what} must 401 without a token");
        assert_eq!(
            response
                .headers()
                .get("www-authenticate")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer"),
            "{what}"
        );
    }
}

#[tokio::test]
async fn sessions_reports_what_the_runner_is_holding() {
    let fixture = boot().await;
    fixture.turns.held.lock().expect("lock").push(SessionInfo {
        context_id: "c1".to_string(),
        session_id: "sess_0001".to_string(),
        cwd: PathBuf::from("/tmp"),
        skill_id: "ask".to_string(),
        idle_secs: 3,
    });

    let payload: Value = reqwest::Client::new()
        .get(format!("{}/sessions", fixture.base))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("GET /sessions")
        .json()
        .await
        .expect("sessions is JSON");

    assert_eq!(payload["retained"], 1);
    assert_eq!(payload["inFlight"], 0);
    assert_eq!(payload["sessions"][0]["contextId"], "c1");
    assert_eq!(payload["sessions"][0]["sessionId"], "sess_0001");
    assert_eq!(payload["sessions"][0]["cwd"], "/tmp");
    assert_eq!(payload["sessions"][0]["skillId"], "ask");
    assert_eq!(payload["sessions"][0]["idleSecs"], 3);
}

#[tokio::test]
async fn deleting_a_session_maps_the_outcome_onto_the_status_code() {
    let fixture = boot().await;
    let client = reqwest::Client::new();
    let url = format!("{}/sessions/c1", fixture.base);
    let close = || client.delete(url.clone()).bearer_auth(TOKEN).send();

    // Nothing is held. 404, and deliberately not treated as a failure: a
    // retried DELETE must be as safe as the first one (the shape S5 recorded
    // for the registry — 200 then 404).
    let response = close().await.expect("DELETE with nothing held");
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"], "no_such_session");
    assert_eq!(body["contextId"], "c1");

    *fixture.turns.closing.lock().expect("lock") = SessionClose::Closed {
        session_id: "sess_0001".to_string(),
    };
    let response = close().await.expect("DELETE with a session held");
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["closed"], true);
    assert_eq!(body["sessionId"], "sess_0001");

    // A turn is running for this context, so the session is checked out and
    // this route must not touch it. The caller is told which request to make
    // instead rather than being left with a 404 that reads like "your
    // conversation is gone".
    *fixture.turns.closing.lock().expect("lock") = SessionClose::Busy;
    let response = close().await.expect("DELETE mid-turn");
    assert_eq!(response.status(), 409);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"], "session_busy");
    assert!(
        body["message"]
            .as_str()
            .expect("message")
            .contains("tasks/cancel"),
        "the refusal must say what to do instead: {body}"
    );
}

#[tokio::test]
async fn the_activity_feed_is_behind_the_token_and_reports_a_turn() {
    // `GET /events` is the operator's view (§activity): it names working
    // directories and mirrors answer sizes, so it sits behind the same bearer
    // layer as the session routes - not in the open control group with
    // `/status`. What it *reports* is the other half: a turn the executor ran,
    // from the request it accepted to how it ended.
    let fixture = boot().await;

    let open = reqwest::Client::new()
        .get(format!("{}/events", fixture.base))
        .send()
        .await
        .expect("GET /events without a token");
    assert_eq!(open.status(), 401, "the feed must 401 without a token");

    // One turn, so the feed has something to report. FakeTurns contributes no
    // steps (it is not an ACP runner); the request and the outcome come from the
    // executor, which is what this file is pinning.
    let card = dial_card(&fixture.base).await;
    let factory = A2AClientFactory::builder()
        .with_interceptor(Arc::new(AuthInterceptor::bearer(TOKEN)))
        .build();
    let client = factory.create_from_card(&card).await.expect("client");
    client.send_text("watch this").await.expect("send");

    let response = reqwest::Client::new()
        .get(format!("{}/events", fixture.base))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("GET /events");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream"),
        "the feed is SSE"
    );

    // Read the backlog a fresh subscriber is owed, and stop once both ends of
    // the turn are seen - the stream is otherwise open forever.
    let mut stream = response.bytes_stream();
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_secs(1), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                seen.push_str(&String::from_utf8_lossy(&chunk));
                if seen.contains("\"type\":\"request_received\"")
                    && seen.contains("\"type\":\"finished\"")
                {
                    break;
                }
            }
            _ => break,
        }
    }

    assert!(
        seen.contains("\"type\":\"request_received\""),
        "the feed must show the request: {seen}"
    );
    assert!(
        seen.contains("\"type\":\"finished\""),
        "the feed must show the outcome: {seen}"
    );
    assert!(
        seen.contains("\"stopReason\":\"end_turn\""),
        "the outcome carries goose's stopReason: {seen}"
    );
    // The turn's identity travels with every event, which is what lets a viewer
    // group a live feed by conversation.
    assert!(
        seen.contains("\"contextId\""),
        "the feed names the context: {seen}"
    );
}
