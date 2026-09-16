//! What the release actually dialled: the path, and the key.
//!
//! Two of the three defects the first host found were on this one hop, and
//! neither was visible from inside the process — a URL only means something once
//! a server sees it arrive.
//!
//! - **The doubled path.** The shipped config spells the endpoint
//!   (`http://127.0.0.1:3284/acp`) and the transport appended `/acp` to it
//!   anyway, so every turn was a `POST /acp/acp` and every turn came back `404`.
//!   The only spelling that worked was the one the config did *not* ship.
//! - **The key that was never sent.** `goose.acp.secretEnv` named a variable
//!   nobody read, so no `X-Secret-Key` ever left this process and a
//!   key-protected `goose serve` answered `401` to everything. The documented
//!   workaround was to run goose unauthenticated.
//!
//! The server here is a fake, and it is fake in exactly the two ways these
//! assertions need: it *records the path* a request arrived on and *the header it
//! carried*, and it answers `404` to anything that is not `/acp`. A fake goose
//! that was forgiving about the path could not fail the way the real one did.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use a2a_goose::acp::{CONNECTION_ID_HEADER, SECRET_HEADER, Transport};

/// One request, as the server saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Dial {
    method: String,
    path: String,
    secret: Option<String>,
}

#[derive(Default)]
struct Seen {
    dials: Mutex<Vec<Dial>>,
}

impl Seen {
    fn record(&self, method: &str, path: &str, headers: &HeaderMap) {
        self.dials
            .lock()
            .expect("the dial log is never held across an await")
            .push(Dial {
                method: method.to_string(),
                path: path.to_string(),
                secret: headers
                    .get(SECRET_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
            });
    }

    fn dials(&self) -> Vec<Dial> {
        self.dials
            .lock()
            .expect("the dial log is never held across an await")
            .clone()
    }

    fn paths(&self) -> Vec<String> {
        self.dials().into_iter().map(|dial| dial.path).collect()
    }
}

/// `initialize`: answers in its body, and the connection id is a *header*, which
/// is the whole reason it is the first call (S3).
async fn post_acp(State(seen): State<Arc<Seen>>, uri: Uri, headers: HeaderMap) -> Response {
    seen.record("POST", uri.path(), &headers);
    Response::builder()
        .status(StatusCode::OK)
        .header(CONNECTION_ID_HEADER, "conn-0001")
        .body(axum::body::Body::from(
            r#"{"jsonrpc":"2.0","id":0,"result":{}}"#,
        ))
        .expect("a well-formed response")
}

/// The connection-level stream. It is opened by `connect`, so a connection that
/// comes back at all has already proven the GET was accepted.
async fn get_acp(State(seen): State<Arc<Seen>>, uri: Uri, headers: HeaderMap) -> Response {
    seen.record("GET", uri.path(), &headers);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(axum::body::Body::from(""))
        .expect("a well-formed response")
}

/// Anything that is not `/acp`. Recorded on the way out so a wrong path shows up
/// in the assertion as the path it was, not only as a `404`.
async fn anything_else(
    State(seen): State<Arc<Seen>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    seen.record(method.as_str(), uri.path(), &headers);
    StatusCode::NOT_FOUND.into_response()
}

/// A fake goose on an ephemeral port, plus the log of what reached it.
async fn fake_goose() -> (String, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let router = Router::new()
        .route("/acp", get(get_acp).post(post_acp))
        .fallback(anything_else)
        .with_state(Arc::clone(&seen));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn the_configured_url_is_dialled_as_the_endpoint_however_it_is_spelled() {
    let (base, seen) = fake_goose().await;

    // The three spellings a host can end up with: the origin, the endpoint the
    // config ships, and the endpoint with a trailing slash.
    for url in [base.clone(), format!("{base}/acp"), format!("{base}/acp/")] {
        Transport::connect(&url, None, Duration::from_secs(5))
            .await
            .unwrap_or_else(|err| panic!("{url} must reach /acp, not {err}"));
    }

    assert_eq!(
        seen.paths(),
        vec![
            "/acp".to_string(),
            "/acp".to_string(),
            "/acp".to_string(),
            "/acp".to_string(),
            "/acp".to_string(),
            "/acp".to_string(),
        ],
        "every spelling dials /acp: initialize and the stream for each of the three"
    );
}

#[tokio::test]
async fn the_key_named_by_secret_env_is_the_key_goose_receives() {
    let (base, seen) = fake_goose().await;

    Transport::connect(
        &base,
        Some("s3cr3t-from-the-environment"),
        Duration::from_secs(5),
    )
    .await
    .expect("initialize");

    let dials = seen.dials();
    assert_eq!(dials.len(), 2, "initialize and the connection stream");
    assert_eq!(dials[0].method, "POST");
    assert_eq!(dials[1].method, "GET");
    for dial in &dials {
        assert_eq!(
            dial.secret.as_deref(),
            Some("s3cr3t-from-the-environment"),
            "a {} to {} must carry {} — the stream and initialize alike, because \
             a header on only one of them is a 401 on the path that forgot",
            dial.method,
            dial.path,
            SECRET_HEADER
        );
    }
}

#[tokio::test]
async fn a_host_with_no_key_sends_no_header_at_all() {
    let (base, seen) = fake_goose().await;

    // Not "refuse to start": whether goose wants a key is goose's answer to
    // give, and it gives it with a 401 that names the header.
    Transport::connect(&base, None, Duration::from_secs(5))
        .await
        .expect("initialize");

    for dial in seen.dials() {
        assert_eq!(
            dial.secret, None,
            "no key configured means no {} header, not an empty one",
            SECRET_HEADER
        );
    }
}
