//! Tests de bout en bout : le vrai routeur, devant une API amont simulée.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use claude_rotate::config::{Account, Config, Loaded};
use claude_rotate::server::{self, App, Storage};
use serde_json::{Value, json};

const DEVICE_KEY: &str = "devkey-1";
const UTIL_5H: &str = "anthropic-ratelimit-unified-5h-utilization";
const STATUS: &str = "anthropic-ratelimit-unified-status";
const RESET_5H: &str = "anthropic-ratelimit-unified-5h-reset";

/// Requête reçue par l'amont simulé.
#[derive(Clone)]
struct Seen {
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

impl Seen {
    fn token(&self) -> &str {
        self.headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("")
    }
}

/// Réponse de l'amont simulé.
struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    chunks: Vec<Bytes>,
    delay: Duration,
}

impl Reply {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: vec![("content-type", "application/json".into())],
            chunks: vec![Bytes::from(body.to_string())],
            delay: Duration::ZERO,
        }
    }

    fn ok() -> Self {
        Self::json(
            200,
            json!({"id": "msg_1", "type": "message", "model": "claude-opus-5",
            "content": [{"type": "text", "text": "bonjour"}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}}),
        )
        .header(UTIL_5H, "0.2")
        .header(STATUS, "allowed")
    }

    fn quota_exhausted() -> Self {
        Self::json(
            429,
            json!({"type": "error", "error": {"type": "rate_limit_error"}}),
        )
        .header(UTIL_5H, "1.0")
        .header(STATUS, "rejected")
    }

    fn sse(chunks: &[&str]) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", "text/event-stream".into())],
            chunks: chunks.iter().map(|c| Bytes::from(c.to_string())).collect(),
            delay: Duration::ZERO,
        }
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

type Handler = dyn Fn(&Seen) -> Reply + Send + Sync;

struct Mock {
    handler: Box<Handler>,
    seen: Mutex<Vec<Seen>>,
}

impl Mock {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn hits(&self, token: &str) -> usize {
        self.seen().iter().filter(|s| s.token() == token).count()
    }
}

async fn mock_handler(State(mock): State<Arc<Mock>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let seen = Seen {
        path: parts.uri.path().to_owned(),
        headers: parts.headers,
        body,
    };
    let reply = (mock.handler)(&seen);
    mock.seen.lock().unwrap().push(seen);
    tokio::time::sleep(reply.delay).await;

    let mut builder = Response::builder().status(reply.status);
    for (name, value) in &reply.headers {
        builder = builder.header(*name, value);
    }
    let stream = futures_util::stream::iter(reply.chunks.into_iter().map(Ok::<_, std::io::Error>));
    builder
        .body(Body::from_stream(stream))
        .unwrap()
        .into_response()
}

async fn spawn(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

struct Harness {
    base: String,
    app: Arc<App>,
    mock: Arc<Mock>,
    http: reqwest::Client,
}

impl Harness {
    async fn new(
        overrides: Value,
        handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let mock = Arc::new(Mock {
            handler: Box::new(handler),
            seen: Mutex::new(Vec::new()),
        });
        let upstream = spawn(
            Router::new()
                .fallback(mock_handler)
                .with_state(Arc::clone(&mock)),
        )
        .await;

        let mut cfg = json!({
            "upstream": upstream,
            "devices": {"laptop": DEVICE_KEY},
            "switch_cooldown_s": 300,
        });
        if let (Value::Object(base), Value::Object(extra)) = (&mut cfg, overrides) {
            base.extend(extra);
        }
        let config: Config = serde_json::from_value(cfg).unwrap();
        let accounts = ["a", "b"]
            .into_iter()
            .map(|name| Account {
                name: name.into(),
                token: format!("tok-{name}"),
                priority: 100,
                disabled: false,
            })
            .collect();
        let app = App::new(Loaded { config, accounts }, Storage::Memory).unwrap();
        let base = spawn(server::router(Arc::clone(&app))).await;
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .unwrap();
        Self {
            base,
            app,
            mock,
            http,
        }
    }

    fn post(&self, path: &str, body: Value) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{path}", self.base))
            .bearer_auth(DEVICE_KEY)
            .header("content-type", "application/json")
            .body(body.to_string())
    }

    async fn messages(&self) -> reqwest::Response {
        self.post(
            "/v1/messages",
            json!({"model": "claude-opus-5", "messages": []}),
        )
        .send()
        .await
        .unwrap()
    }

    fn switches(&self) -> usize {
        self.app
            .state()
            .events
            .iter()
            .filter(|e| e.kind == "switch")
            .count()
    }
}

#[tokio::test]
async fn unknown_device_key_is_rejected_before_upstream() {
    let h = Harness::new(json!({}), |_| Reply::ok()).await;
    let resp = h
        .http
        .post(format!("{}/v1/messages", h.base))
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"]["message"],
        "claude-rotate: unknown device key"
    );
    assert!(h.mock.seen().is_empty());
}

#[tokio::test]
async fn headers_are_rewritten_transparently() {
    let h = Harness::new(json!({}), |_| {
        Reply::ok().header("content-encoding", "gzip")
    })
    .await;
    let resp = h
        .post("/v1/messages?beta=true", json!({"model": "claude-opus-5"}))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("x-api-key", DEVICE_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-rotate-account"], "a");
    assert!(resp.headers().get("content-encoding").is_none());

    let seen = &h.mock.seen()[0];
    assert_eq!(seen.path, "/v1/messages");
    assert_eq!(seen.token(), "tok-a");
    assert_eq!(seen.headers["accept-encoding"], "identity");
    assert_eq!(seen.headers["anthropic-beta"], "oauth-2025-04-20");
    assert!(seen.headers.get("x-api-key").is_none());

    assert_eq!(h.app.audit().len(), 1);
}

#[tokio::test]
async fn sse_is_relayed_byte_for_byte_and_audited() {
    let chunks = [
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":",
        "{\"input_tokens\":7,\"output_tokens\":1}}}\n\nevent: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"h\u{e9}\"}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n",
    ];
    let expected = chunks.concat();
    let h = Harness::new(json!({}), move |_| Reply::sse(&chunks)).await;
    let body = h.messages().await.text().await.unwrap();
    assert_eq!(body, expected);

    // L'audit est écrit à la fin du flux, côté serveur.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.app.audit().len(), 1);
}

#[tokio::test]
async fn quota_429_rotates_and_retries_transparently() {
    let h = Harness::new(json!({}), |seen| match seen.token() {
        "tok-a" => Reply::quota_exhausted(),
        _ => Reply::ok(),
    })
    .await;
    let resp = h.messages().await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-rotate-account"], "b");
    assert_eq!(h.app.state().active_account, "b");
    assert_eq!(h.switches(), 1);
    assert_eq!(h.app.state().events[0].reason, "quota_exhausted");
}

#[tokio::test]
async fn burst_429_paces_on_the_same_account() {
    let first = Arc::new(Mutex::new(true));
    let h = Harness::new(json!({}), move |_| {
        let mut first = first.lock().unwrap();
        if std::mem::take(&mut *first) {
            Reply::json(429, json!({}))
                .header(UTIL_5H, "0.3")
                .header("retry-after", "0")
        } else {
            Reply::ok()
        }
    })
    .await;
    let resp = h.messages().await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-rotate-account"], "a");
    assert_eq!(h.switches(), 0);
    assert_eq!(h.mock.hits("tok-a"), 2);
}

#[tokio::test]
async fn exhausted_passes_429_through_and_openai_gets_503() {
    let h = Harness::new(json!({}), |_| Reply::quota_exhausted()).await;
    let resp = h.messages().await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    let resp = h
        .post(
            "/v1/chat/completions",
            json!({"model": "claude-sonnet-5", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let retry_after: u64 = resp.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry_after >= 1);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn hold_until_reset_releases_requests_together() {
    // Comme l'API réelle : une heure de réinitialisation fixe pour la fenêtre.
    let started = Instant::now();
    let reset = claude_rotate::time::now() + 0.5;
    let h = Harness::new(json!({"hold_max_s": 3}), move |_| {
        if started.elapsed() < Duration::from_millis(800) {
            Reply::quota_exhausted().header(RESET_5H, reset.to_string())
        } else {
            Reply::ok()
        }
    })
    .await;

    let t0 = Instant::now();
    let responses = futures_util::future::join_all((0..10).map(|_| h.messages())).await;
    assert!(responses.iter().all(|r| r.status() == StatusCode::OK));
    assert!(
        t0.elapsed() >= Duration::from_millis(800),
        "les requêtes ont bien été retenues"
    );
    assert!(h.app.state().events.iter().any(|e| e.kind == "hold"));
}

#[tokio::test]
async fn herd_hitting_a_quota_causes_exactly_one_switch() {
    const RACERS: usize = 100;
    let h = Harness::new(json!({}), |seen| match seen.token() {
        "tok-a" => Reply::quota_exhausted().delay(Duration::from_millis(50)),
        _ => Reply::ok(),
    })
    .await;

    let responses = futures_util::future::join_all((0..RACERS).map(|_| h.messages())).await;
    assert!(responses.iter().all(|r| r.status() == StatusCode::OK));
    assert_eq!(
        h.switches(),
        1,
        "une seule décision de bascule pour tout le troupeau"
    );
    assert!(
        h.mock.hits("tok-a") <= RACERS,
        "chaque requête touche le compte mourant au plus une fois"
    );
    assert_eq!(
        h.mock.hits("tok-b"),
        RACERS,
        "une seule nouvelle tentative par requête"
    );
}

#[tokio::test]
async fn openai_endpoint_translates_both_ways() {
    let h = Harness::new(json!({}), |_| Reply::ok()).await;
    let resp = h
        .post(
            "/v1/chat/completions",
            json!({"model": "claude-opus-5", "max_tokens": 64, "messages": [
                {"role": "system", "content": "sois bref"},
                {"role": "user", "content": "bonjour"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-rotate-account"], "a");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "bonjour");
    assert_eq!(body["usage"]["total_tokens"], 15);

    let seen = &h.mock.seen()[0];
    assert_eq!(seen.path, "/v1/messages");
    assert_eq!(seen.headers["anthropic-beta"], "oauth-2025-04-20");
    let sent: Value = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(sent["system"], "sois bref");
    assert_eq!(sent["max_tokens"], 64);
}

#[tokio::test]
async fn openai_streaming_emits_chunks_and_done() {
    let h = Harness::new(json!({}), |_| {
        Reply::sse(&[
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_9\",\"model\":\"m\",\"usage\":{\"input_tokens\":3}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"salut\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        ])
    })
    .await;
    let body = h
        .post(
            "/v1/chat/completions",
            json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "x"}]}),
        )
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("\"content\":\"salut\""), "{body}");
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
}

#[tokio::test]
async fn openai_endpoint_refuses_tools() {
    let h = Harness::new(json!({}), |_| Reply::ok()).await;
    let resp = h
        .post(
            "/v1/chat/completions",
            json!({"model": "m", "tools": [{"type": "function"}], "messages": [{"role": "user", "content": "x"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(h.mock.seen().is_empty());
}

#[tokio::test]
async fn admin_endpoints_require_a_device_key() {
    let h = Harness::new(json!({}), |_| Reply::ok()).await;
    for path in ["/rotate/status", "/rotate/stats", "/rotate/panel"] {
        let resp = h
            .http
            .get(format!("{}{path}", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{path}");
    }

    h.messages().await;
    let status: Value = h
        .http
        .get(format!("{}/rotate/status?key={DEVICE_KEY}", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["active_account"], "a");
    assert_eq!(status["accounts"]["a"]["util_5h"], 0.2);
    assert_eq!(status["strategy"], "consume-first");

    let stats: Value = h
        .http
        .get(format!("{}/rotate/stats", h.base))
        .bearer_auth(DEVICE_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["by_device"]["laptop"]["requests"], 1);

    let panel = h
        .http
        .get(format!("{}/rotate/panel?key={DEVICE_KEY}", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(panel.status(), StatusCode::OK);
    assert_eq!(panel.headers()["referrer-policy"], "no-referrer");
}

#[tokio::test]
async fn upstream_down_yields_502() {
    let config: Config = serde_json::from_value(json!({
        "upstream": "http://127.0.0.1:9",
        "devices": {"laptop": DEVICE_KEY},
    }))
    .unwrap();
    let accounts = vec![Account {
        name: "a".into(),
        token: "tok-a".into(),
        priority: 100,
        disabled: false,
    }];
    let app = App::new(Loaded { config, accounts }, Storage::Memory).unwrap();
    let base = spawn(server::router(app)).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .bearer_auth(DEVICE_KEY)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}
