//! Serveur HTTP : authentification des appareils, relais vers l'API amont,
//! boucle de rotation partagée par le proxy transparent et l'endpoint OpenAI.
//!
//! Faits porteurs, vérifiés sur l'API réelle par l'implémentation d'origine :
//! 1. relais transparent — seul l'en-tête `authorization` est remplacé, les
//!    en-têtes du client (`anthropic-beta`, etc.) passent tels quels ;
//! 2. `accept-encoding: identity` en amont et `content-encoding` retiré de la
//!    réponse, sinon le client tente de décompresser un corps déjà en clair ;
//! 3. la télémétrie de quota vient des en-têtes `anthropic-ratelimit-unified-*` ;
//! 4. l'usage des flux SSE est lu au passage, sans modifier un seul octet.

use std::collections::HashMap;
use std::future::IntoFuture;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio::sync::Notify;

use crate::audit::{AuditLog, AuditRecord, StatsContext};
use crate::config::{AUDIT_FILE, AlertConfig, Config, Loaded, STATE_FILE};
use crate::oai::{self, SseToOai};
use crate::rotation::{
    AccountMeta, Event, Policy, RateHeaders, Rotator, State as RotationState, Verdict,
    headers as rl,
};
use crate::time::{local_timestamp, now};
use crate::usage::{SseUsageScanner, Usage};

/// Nombre maximal de nouvelles tentatives après une bascule ou une rafale.
const MAX_RETRIES: u32 = 5;
/// Pause maximale avant d'envoyer sur un compte en cours de temporisation.
const MAX_PACE: Duration = Duration::from_secs(60);
/// Marge ajoutée après la réinitialisation attendue avant de relancer une requête retenue.
const HOLD_SLACK_S: f64 = 5.0;
/// Taille maximale d'un corps de requête (les conversations avec images sont lourdes).
const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;
/// Délai laissé aux requêtes en cours lors d'un arrêt.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// Nombre d'événements renvoyés par `/rotate/status`.
const STATUS_EVENTS: usize = 10;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const PANEL_HTML: &str = include_str!("panel.html");

/// En-têtes propres à une connexion, jamais relayés (dans un sens comme dans l'autre).
const HOP_BY_HOP: [&str; 11] = [
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "accept-encoding",
];

/// En-têtes de quota recopiés sur les réponses de l'endpoint OpenAI.
const OAI_TELEMETRY: [&str; 4] = [rl::UTIL_5H, rl::UTIL_7D, rl::RESET_5H, rl::STATUS];

static X_ROTATE_ACCOUNT: HeaderName = HeaderName::from_static("x-rotate-account");

/// Où l'état et l'audit sont conservés.
#[derive(Debug, Clone)]
pub enum Storage {
    /// `state.json` et `logs/audit.jsonl` sous ce répertoire.
    Disk(PathBuf),
    /// Rien n'est écrit (tests).
    Memory,
}

/// État partagé du serveur.
pub struct App {
    /// clé d'appareil -> nom.
    device_by_key: HashMap<String, String>,
    /// nom -> clé, dans l'ordre de la configuration (roster du panel).
    devices: IndexMap<String, String>,
    /// compte -> `Bearer <jeton>` prêt à l'emploi, marqué sensible.
    auth_headers: HashMap<String, HeaderValue>,
    rotator: Mutex<Rotator>,
    hold_max_s: f64,
    prices: IndexMap<String, [f64; 2]>,
    alerts: AlertConfig,
    upstream: String,
    http: reqwest::Client,
    audit: AuditLog,
    state_file: Option<StateFile>,
}

/// Persistance de `state.json` : les écritures sont confiées à une tâche de
/// fond et regroupées, pour ne jamais faire d'E/S sous le verrou de rotation.
struct StateFile {
    path: PathBuf,
    dirty: Notify,
    write_lock: Mutex<()>,
}

/// Réponse amont retenue par la boucle de rotation.
struct Upstream {
    resp: reqwest::Response,
    account: String,
    retries: u32,
    /// `note_response` a déjà intégré cette réponse (un 429 final).
    noted: bool,
    /// Conclusion tirée du dernier 429 rencontré.
    verdict: Option<Verdict>,
}

impl App {
    /// Construit l'état du serveur. Avec [`Storage::Disk`], doit être appelé
    /// depuis un runtime tokio (tâches d'écriture de l'état et de l'audit).
    pub fn new(loaded: Loaded, storage: Storage) -> Result<Arc<Self>> {
        let Loaded { config, accounts } = loaded;

        let mut auth_headers = HashMap::with_capacity(accounts.len());
        for acct in &accounts {
            let mut value = HeaderValue::from_str(&format!("Bearer {}", acct.token))
                .with_context(|| format!("jeton du compte '{}' invalide", acct.name))?;
            value.set_sensitive(true);
            auth_headers.insert(acct.name.clone(), value);
        }

        let (state, audit, state_file) = match storage {
            Storage::Disk(root) => {
                let path = root.join(STATE_FILE);
                let audit = AuditLog::open(root.join(AUDIT_FILE))?;
                let state_file = StateFile {
                    path: path.clone(),
                    dirty: Notify::new(),
                    write_lock: Mutex::new(()),
                };
                (load_state(&path), audit, Some(state_file))
            }
            Storage::Memory => (RotationState::default(), AuditLog::in_memory(), None),
        };

        let metas = accounts.iter().map(AccountMeta::from).collect();
        let rotator = Rotator::new(Policy::from(&config), metas, state);

        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(600))
            .build()
            .context("création du client HTTP amont")?;

        let app = Arc::new(Self {
            device_by_key: config
                .devices
                .iter()
                .map(|(n, k)| (k.clone(), n.clone()))
                .collect(),
            devices: config.devices.clone(),
            auth_headers,
            rotator: Mutex::new(rotator),
            hold_max_s: config.hold_max_s,
            prices: config.prices_per_mtok.clone(),
            alerts: config.alerts.clone(),
            upstream: config.upstream.trim_end_matches('/').to_owned(),
            http,
            audit,
            state_file,
        });
        if app.state_file.is_some() {
            tokio::spawn(save_state_loop(Arc::clone(&app)));
        }
        Ok(app)
    }

    fn rotator(&self) -> MutexGuard<'_, Rotator> {
        // Un panic sous le verrou ne laisse pas l'état incohérent (chaque
        // mutation est atomique) : on continue avec la dernière valeur.
        self.rotator.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Accès en lecture au journal d'audit.
    pub fn audit(&self) -> &AuditLog {
        &self.audit
    }

    /// Instantané de l'état de rotation.
    pub fn state(&self) -> RotationState {
        self.rotator().state().clone()
    }

    /// Identifie l'appareil : `Authorization: Bearer`, puis `x-api-key`, puis `?key=`.
    fn device(&self, headers: &HeaderMap, uri: &Uri) -> Option<String> {
        let header_key = |name: &HeaderName| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|k| !k.is_empty())
        };
        let bearer = header_key(&header::AUTHORIZATION).and_then(|auth| {
            let (scheme, key) = auth.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| key.trim())
        });
        let key = match bearer.or_else(|| header_key(&HeaderName::from_static("x-api-key"))) {
            Some(key) => key.to_owned(),
            None => Query::<HashMap<String, String>>::try_from_uri(uri)
                .ok()
                .and_then(|Query(mut q)| q.remove("key"))?,
        };
        self.device_by_key.get(&key).cloned()
    }

    /// Intègre une réponse amont dans la politique de rotation.
    fn note(&self, account: &str, rate: &RateHeaders, status: u16) -> Option<Verdict> {
        let verdict = {
            let mut rotator = self.rotator();
            let before = rotator.active().to_owned();
            let verdict = rotator.note_response(account, rate, status, now());
            if rotator.active() != before {
                let reason = rotator
                    .recent_events(1)
                    .first()
                    .map_or("", |e| e.reason.as_str());
                tracing::info!(from = %before, to = %rotator.active(), reason, "bascule de compte");
            }
            verdict
        };
        self.mark_dirty();
        verdict
    }

    fn mark_dirty(&self) {
        if let Some(file) = &self.state_file {
            file.dirty.notify_one();
        }
    }

    /// Écrit immédiatement `state.json` (arrêt du serveur).
    fn save_state_now(&self) {
        if let Some(file) = &self.state_file {
            let snapshot = self.state_snapshot();
            file.write(&snapshot);
        }
    }

    fn state_snapshot(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self.rotator().state()).unwrap_or_default()
    }

    /// Envoie la requête sur le compte actif, en suivant les bascules, en
    /// temporisant les rafales et, si `hold_max_s` le permet, en retenant la
    /// requête jusqu'à la prochaine réinitialisation quand tout est épuisé.
    async fn send_upstream(
        &self,
        method: &Method,
        path_and_query: &str,
        mut headers: HeaderMap,
        body: Bytes,
    ) -> reqwest::Result<Upstream> {
        let url = format!("{}{}", self.upstream, path_and_query);
        let hold_deadline = (self.hold_max_s > 0.0).then(|| now() + self.hold_max_s);
        let mut retries = 0;
        loop {
            let (account, pace) = {
                let rotator = self.rotator();
                let account = rotator.active().to_owned();
                let pace = rotator.pace_remaining(&account, now());
                (account, pace)
            };
            // Invariant : le compte actif est toujours un compte configuré.
            let auth = self.auth_headers.get(&account).cloned().unwrap_or_else(|| {
                unreachable!("compte actif '{account}' absent de la configuration")
            });
            headers.insert(header::AUTHORIZATION, auth);
            if pace > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(pace).min(MAX_PACE)).await;
            }

            let resp = self
                .http
                .request(method.clone(), &url)
                .headers(headers.clone())
                .body(body.clone())
                .send()
                .await?;
            if resp.status() != StatusCode::TOO_MANY_REQUESTS {
                return Ok(Upstream {
                    resp,
                    account,
                    retries,
                    noted: false,
                    verdict: None,
                });
            }

            let verdict = self.note(&account, &RateHeaders::from_headers(resp.headers()), 429);
            let mut retry = matches!(verdict, Some(Verdict::Switched | Verdict::Burst))
                && retries < MAX_RETRIES;
            let mut wait = 0.0;
            if let (false, Some(Verdict::Exhausted), Some(deadline)) =
                (retry, verdict, hold_deadline)
            {
                let relief = self.rotator().earliest_relief(now());
                wait = (relief + HOLD_SLACK_S).min(deadline) - now();
                retry = wait > 0.0;
            }
            if !retry {
                return Ok(Upstream {
                    resp,
                    account,
                    retries,
                    noted: true,
                    verdict,
                });
            }

            drop(resp);
            retries += 1;
            if wait > 0.0 {
                self.rotator().push_event(Event {
                    ts: local_timestamp(),
                    kind: "hold".into(),
                    from: account.clone(),
                    to: account.clone(),
                    reason: format!("all accounts spent; holding {}s", wait.round()),
                    util: None,
                });
                self.mark_dirty();
                tracing::info!(account = %account, wait_s = wait.round(), "tous les comptes épuisés, requête retenue");
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            }
        }
    }

    fn status_json(&self) -> Value {
        let now = now();
        let rotator = self.rotator();
        let accounts: Map<String, Value> = rotator
            .accounts()
            .iter()
            .map(|meta| {
                let state = rotator
                    .account_state(&meta.name)
                    .cloned()
                    .unwrap_or_default();
                let mut info = match serde_json::to_value(&state) {
                    Ok(Value::Object(map)) => map,
                    _ => Map::new(),
                };
                info.insert(
                    "effective_util_5h".into(),
                    json!(rotator.util_5h(&meta.name, now)),
                );
                info.insert(
                    "effective_util_7d".into(),
                    json!(rotator.util_7d(&meta.name, now)),
                );
                info.insert("priority".into(), json!(meta.priority));
                info.insert("disabled".into(), json!(meta.disabled));
                if let Some(reset) = state.reset_5h.filter(|r| *r != 0.0) {
                    info.insert(
                        "reset_5h_in_min".into(),
                        json!(((reset - now) / 60.0).round().max(0.0)),
                    );
                }
                if let Some(reset) = state.reset_7d.filter(|r| *r != 0.0) {
                    let hours = ((reset - now) / 3600.0 * 10.0).round() / 10.0;
                    info.insert("reset_7d_in_h".into(), json!(hours.max(0.0)));
                }
                (meta.name.clone(), Value::Object(info))
            })
            .collect();
        let policy = rotator.policy();
        json!({
            "active_account": rotator.active(),
            "switch_threshold": policy.threshold,
            "switch_threshold_7d": policy.threshold_7d,
            "strategy": policy.strategy,
            "hold_max_s": self.hold_max_s,
            "accounts": accounts,
            "recent_events": rotator.recent_events(STATUS_EVENTS),
        })
    }

    /// Vide l'audit et enregistre l'état ; appelé une fois le serveur arrêté.
    pub async fn shutdown(&self) {
        self.audit.shutdown().await;
        self.save_state_now();
    }
}

impl StateFile {
    /// Écriture atomique (fichier temporaire puis renommage) : un arrêt brutal
    /// ne peut pas laisser un `state.json` tronqué.
    fn write(&self, data: &[u8]) {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let tmp = self.path.with_extension("json.tmp");
        let result = std::fs::write(&tmp, data).and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(err) = result {
            tracing::warn!(path = %self.path.display(), %err, "écriture de l'état impossible");
        }
    }
}

async fn save_state_loop(app: Arc<App>) {
    let Some(file) = &app.state_file else { return };
    loop {
        // Les notifications reçues pendant une écriture fusionnent en une seule.
        file.dirty.notified().await;
        let app = Arc::clone(&app);
        let joined = tokio::task::spawn_blocking(move || {
            let snapshot = app.state_snapshot();
            if let Some(file) = &app.state_file {
                file.write(&snapshot);
            }
        })
        .await;
        if let Err(err) = joined {
            tracing::warn!(%err, "tâche d'écriture de l'état interrompue");
        }
    }
}

/// Un `state.json` illisible n'est pas bloquant : la télémétrie se réapprend
/// dès les premières réponses, seul l'historique des événements est perdu.
fn load_state(path: &Path) -> RotationState {
    match std::fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|err| {
            tracing::warn!(path = %path.display(), %err, "état illisible, on repart de zéro");
            RotationState::default()
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => RotationState::default(),
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "état illisible, on repart de zéro");
            RotationState::default()
        }
    }
}

/// Routeur complet du proxy.
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/rotate/status", get(rotate_status))
        .route("/rotate/stats", get(rotate_stats))
        .route("/rotate/panel", get(rotate_panel))
        .route("/v1/chat/completions", post(openai_chat))
        .fallback(proxy)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(app)
}

/// Charge la configuration sous `root` et sert jusqu'à Ctrl-C / SIGTERM.
pub async fn run(root: &Path) -> Result<()> {
    let loaded = Config::load(root)?;
    let addr = format!("{}:{}", loaded.config.bind, loaded.config.port);
    let (n_accounts, n_devices) = (loaded.accounts.len(), loaded.config.devices.len());
    let app = App::new(loaded, Storage::Disk(root.to_path_buf()))?;

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("écoute sur {addr} impossible"))?;
    tracing::info!(%addr, accounts = n_accounts, devices = n_devices, "claude-rotate démarré");

    let stop = Arc::new(Notify::new());
    let stop_rx = Arc::clone(&stop);
    let server = axum::serve(listener, router(Arc::clone(&app)))
        .with_graceful_shutdown(async move { stop_rx.notified().await })
        .into_future();
    let mut server = std::pin::pin!(server);

    tokio::select! {
        result = &mut server => result.context("serveur HTTP")?,
        () = shutdown_signal() => {
            tracing::info!("arrêt demandé, fin des requêtes en cours");
            stop.notify_one();
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut server).await {
                Ok(result) => result.context("serveur HTTP")?,
                Err(_) => tracing::warn!("requêtes encore ouvertes après le délai de grâce, arrêt forcé"),
            }
        }
    }
    app.shutdown().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(%err, "écoute de Ctrl-C impossible");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::warn!(%err, "écoute de SIGTERM impossible");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

// ---------------------------------------------------------------------------
// Endpoints d'administration
// ---------------------------------------------------------------------------

fn unknown_device(message: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": message }))).into_response()
}

async fn rotate_status(State(app): State<Arc<App>>, headers: HeaderMap, uri: Uri) -> Response {
    if app.device(&headers, &uri).is_none() {
        return unknown_device("unknown device key");
    }
    Json(app.status_json()).into_response()
}

async fn rotate_stats(State(app): State<Arc<App>>, headers: HeaderMap, uri: Uri) -> Response {
    if app.device(&headers, &uri).is_none() {
        return unknown_device("unknown device key");
    }
    let (threshold, account_utils) = {
        let rotator = app.rotator();
        let now = now();
        let utils = rotator
            .accounts()
            .iter()
            .map(|m| (m.name.clone(), rotator.util_5h(&m.name, now)))
            .collect();
        (rotator.policy().threshold, utils)
    };
    let ctx = StatsContext {
        devices: &app.devices,
        prices: &app.prices,
        alerts: &app.alerts,
        threshold,
        account_utils,
    };
    Json(app.audit.stats(&ctx, now())).into_response()
}

async fn rotate_panel(State(app): State<Arc<App>>, headers: HeaderMap, uri: Uri) -> Response {
    if app.device(&headers, &uri).is_none() {
        return unknown_device("unknown device key — open /rotate/panel?key=<device key>");
    }
    let mut resp = Html(PANEL_HTML).into_response();
    let h = resp.headers_mut();
    // La clé circule dans l'URL : elle ne doit fuir ni par le Referer ni par un cache.
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    resp
}

// ---------------------------------------------------------------------------
// Proxy transparent
// ---------------------------------------------------------------------------

fn anthropic_error(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = json!({ "type": "error", "error": { "type": kind, "message": message } });
    (status, Json(body)).into_response()
}

/// En-têtes du client à relayer : tout sauf les en-têtes de connexion et les
/// identifiants de l'appareil, qui s'arrêtent ici.
fn forward_request_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(src.len());
    for (name, value) in src {
        let n = name.as_str();
        if !HOP_BY_HOP.contains(&n) && n != "x-api-key" && name != header::AUTHORIZATION {
            out.append(name.clone(), value.clone());
        }
    }
    out.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    out
}

/// En-têtes de la réponse amont à renvoyer au client.
fn response_headers(src: &HeaderMap, account: &str) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(src.len() + 1);
    for (name, value) in src {
        if !HOP_BY_HOP.contains(&name.as_str()) && name != header::CONTENT_ENCODING {
            out.append(name.clone(), value.clone());
        }
    }
    insert_account(&mut out, account);
    out
}

fn insert_account(headers: &mut HeaderMap, account: &str) {
    if let Ok(value) = HeaderValue::from_str(account) {
        headers.insert(X_ROTATE_ACCOUNT.clone(), value);
    }
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"))
}

fn upstream_unreachable(err: &reqwest::Error) -> Response {
    tracing::warn!(%err, "API amont injoignable");
    anthropic_error(
        StatusCode::BAD_GATEWAY,
        "api_error",
        "claude-rotate: upstream unreachable",
    )
}

async fn proxy(State(app): State<Arc<App>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let Some(device) = app.device(&parts.headers, &parts.uri) else {
        return anthropic_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "claude-rotate: unknown device key",
        );
    };
    let body = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return anthropic_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "claude-rotate: request body unreadable or too large",
            );
        }
    };

    let model = if parts.method == Method::POST {
        model_of(&body)
    } else {
        None
    };
    let path = parts.uri.path().to_owned();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or(path.as_str(), |pq| pq.as_str());

    let up = match app
        .send_upstream(
            &parts.method,
            path_and_query,
            forward_request_headers(&parts.headers),
            body,
        )
        .await
    {
        Ok(up) => up,
        Err(err) => return upstream_unreachable(&err),
    };

    let status = up.resp.status();
    let headers = response_headers(up.resp.headers(), &up.account);
    let rate = RateHeaders::from_headers(up.resp.headers());
    let mut rec = AuditRecord {
        device,
        account: up.account.clone(),
        path,
        model,
        status: status.as_u16(),
        retries: up.retries,
        util_5h: header_string(up.resp.headers(), rl::UTIL_5H),
        ..AuditRecord::default()
    };

    if is_event_stream(up.resp.headers()) {
        let body = relay_stream(Arc::clone(&app), up.resp, rec, rate, Passthrough::default());
        return (status, headers, body).into_response();
    }

    let content = match up.resp.bytes().await {
        Ok(content) => content,
        Err(err) => return upstream_unreachable(&err),
    };
    rec.usage = serde_json::from_slice::<Value>(&content)
        .ok()
        .and_then(|v| v.get("usage").and_then(Usage::from_json));
    if rec.path.starts_with("/v1/messages") || rec.usage.is_some() {
        app.audit.record(rec);
    }
    if !up.noted {
        app.note(&up.account, &rate, status.as_u16());
    }
    (status, headers, content).into_response()
}

fn model_of(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value.get("model")?.as_str().map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Relais des flux SSE
// ---------------------------------------------------------------------------

/// Transformation appliquée à un flux SSE relayé.
trait SseTransform: Send + 'static {
    /// Octets à renvoyer au client pour ce fragment amont (vide = rien).
    fn feed(&mut self, chunk: Bytes) -> Bytes;
    /// Octets à émettre une fois le flux amont terminé normalement.
    fn finish(&mut self) -> Bytes;
    fn usage(&self) -> Option<Usage>;
}

/// Proxy transparent : chaque octet passe tel quel, l'usage est lu au passage.
#[derive(Default)]
struct Passthrough(SseUsageScanner);

impl SseTransform for Passthrough {
    fn feed(&mut self, chunk: Bytes) -> Bytes {
        self.0.feed(&chunk);
        chunk
    }
    fn finish(&mut self) -> Bytes {
        Bytes::new()
    }
    fn usage(&self) -> Option<Usage> {
        self.0.usage()
    }
}

impl SseTransform for SseToOai {
    fn feed(&mut self, chunk: Bytes) -> Bytes {
        Bytes::from(SseToOai::feed(self, &chunk))
    }
    fn finish(&mut self) -> Bytes {
        Bytes::from(self.tail())
    }
    fn usage(&self) -> Option<Usage> {
        SseToOai::usage(self)
    }
}

/// Termine l'audit d'un flux quand il s'arrête — fin normale, erreur amont ou
/// client parti en cours de route (le flux est alors simplement abandonné).
struct StreamAudit {
    app: Arc<App>,
    rec: Option<AuditRecord>,
    rate: RateHeaders,
}

impl StreamAudit {
    fn set_usage(&mut self, usage: Option<Usage>) {
        if let Some(rec) = &mut self.rec {
            rec.usage = usage;
        }
    }
}

impl Drop for StreamAudit {
    fn drop(&mut self) {
        if let Some(rec) = self.rec.take() {
            let (account, status) = (rec.account.clone(), rec.status);
            self.app.audit.record(rec);
            self.app.note(&account, &self.rate, status);
        }
    }
}

fn relay_stream(
    app: Arc<App>,
    resp: reqwest::Response,
    rec: AuditRecord,
    rate: RateHeaders,
    mut transform: impl SseTransform,
) -> Body {
    let mut audit = StreamAudit {
        app,
        rec: Some(rec),
        rate,
    };
    let mut upstream = resp.bytes_stream();
    Body::from_stream(async_stream::stream! {
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(chunk) => {
                    let out = transform.feed(chunk);
                    audit.set_usage(transform.usage());
                    if !out.is_empty() {
                        yield Ok::<Bytes, std::io::Error>(out);
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, "flux amont interrompu");
                    yield Err(std::io::Error::other(err));
                    return;
                }
            }
        }
        let tail = transform.finish();
        audit.set_usage(transform.usage());
        if !tail.is_empty() {
            yield Ok(tail);
        }
    })
}

// ---------------------------------------------------------------------------
// Endpoint compatible OpenAI
// ---------------------------------------------------------------------------

fn oai_error(status: StatusCode, kind: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "message": message, "type": kind } })),
    )
        .into_response()
}

/// `POST /v1/chat/completions` : n'importe quel client au format OpenAI profite
/// des comptes en rotation. Aucun en-tête du client n'est relayé. Quand tous
/// les comptes sont épuisés, renvoie 503 + `Retry-After` (signal exploitable
/// par le disjoncteur d'un routeur), contrairement au proxy transparent qui
/// laisse passer le 429 d'Anthropic, que Claude Code gère nativement.
async fn openai_chat(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let Some(device) = app.device(&headers, &uri) else {
        return oai_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "claude-rotate: unknown device key",
        );
    };
    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
        return oai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "body is not valid JSON",
        );
    };
    let anthropic_body = match oai::oai_to_anthropic(&request) {
        Ok(body) => body,
        Err(msg) => return oai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &msg),
    };
    let streaming = anthropic_body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let model = anthropic_body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let payload = match serde_json::to_vec(&anthropic_body) {
        Ok(payload) => Bytes::from(payload),
        Err(err) => {
            return oai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                &err.to_string(),
            );
        }
    };

    let mut fwd = HeaderMap::new();
    fwd.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    fwd.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    fwd.insert(
        "anthropic-version",
        HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    fwd.insert("anthropic-beta", HeaderValue::from_static(OAUTH_BETA));

    let up = match app
        .send_upstream(&Method::POST, "/v1/messages", fwd, payload)
        .await
    {
        Ok(up) => up,
        Err(err) => {
            tracing::warn!(%err, "API amont injoignable");
            return oai_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "claude-rotate: upstream unreachable",
            );
        }
    };

    let status = up.resp.status();
    let mut out_headers = HeaderMap::new();
    insert_account(&mut out_headers, &up.account);
    for name in OAI_TELEMETRY {
        if let Some(value) = up.resp.headers().get(name) {
            out_headers.insert(name, value.clone());
        }
    }
    let rate = RateHeaders::from_headers(up.resp.headers());
    let mut rec = AuditRecord {
        device,
        account: up.account.clone(),
        path: "/v1/chat/completions".into(),
        model,
        status: status.as_u16(),
        retries: up.retries,
        util_5h: header_string(up.resp.headers(), rl::UTIL_5H),
        ..AuditRecord::default()
    };

    if status == StatusCode::TOO_MANY_REQUESTS && up.verdict == Some(Verdict::Exhausted) {
        let retry_after = (app.rotator().earliest_relief(now()) - now())
            .round()
            .max(1.0) as u64;
        rec.status = StatusCode::SERVICE_UNAVAILABLE.as_u16();
        app.audit.record(rec);
        out_headers.insert(header::RETRY_AFTER, HeaderValue::from(retry_after));
        let message =
            format!("claude-rotate: every account's window is spent; retry in {retry_after}s");
        let mut resp = oai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            &message,
        );
        resp.headers_mut().extend(out_headers);
        return resp;
    }

    if streaming && status == StatusCode::OK && is_event_stream(up.resp.headers()) {
        out_headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        out_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        let body = relay_stream(Arc::clone(&app), up.resp, rec, rate, SseToOai::new());
        return (StatusCode::OK, out_headers, body).into_response();
    }

    let content = match up.resp.bytes().await {
        Ok(content) => content,
        Err(err) => {
            tracing::warn!(%err, "lecture de la réponse amont impossible");
            return oai_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "claude-rotate: upstream read failed",
            );
        }
    };
    if !up.noted {
        app.note(&up.account, &rate, status.as_u16());
    }

    if status != StatusCode::OK {
        app.audit.record(rec);
        let mut resp = oai_error(
            status,
            "api_error",
            &format!("upstream: {}", upstream_message(&content)),
        );
        resp.headers_mut().extend(out_headers);
        return resp;
    }

    let Ok(message) = serde_json::from_slice::<Value>(&content) else {
        app.audit.record(rec);
        return oai_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "upstream returned invalid JSON",
        );
    };
    rec.usage = message.get("usage").and_then(Usage::from_json);
    app.audit.record(rec);
    (out_headers, Json(oai::anthropic_to_oai(&message))).into_response()
}

/// Message d'erreur lisible extrait d'une réponse amont en échec.
fn upstream_message(content: &[u8]) -> String {
    serde_json::from_slice::<Value>(content)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(&content[..content.len().min(200)]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_headers_drop_credentials_and_force_identity() {
        let mut src = HeaderMap::new();
        src.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer dev-key"),
        );
        src.insert("x-api-key", HeaderValue::from_static("dev-key"));
        src.insert(header::HOST, HeaderValue::from_static("proxy:8484"));
        src.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip, br"),
        );
        src.insert(
            "anthropic-beta",
            HeaderValue::from_static("oauth-2025-04-20"),
        );
        let out = forward_request_headers(&src);
        assert!(out.get(header::AUTHORIZATION).is_none());
        assert!(out.get("x-api-key").is_none());
        assert!(out.get(header::HOST).is_none());
        assert_eq!(out[header::ACCEPT_ENCODING], "identity");
        assert_eq!(out["anthropic-beta"], "oauth-2025-04-20");
    }

    #[test]
    fn response_headers_drop_encoding_and_tag_account() {
        let mut src = HeaderMap::new();
        src.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        src.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        src.insert("request-id", HeaderValue::from_static("req_1"));
        let out = response_headers(&src, "acct-1");
        assert!(out.get(header::CONTENT_ENCODING).is_none());
        assert!(out.get(header::TRANSFER_ENCODING).is_none());
        assert_eq!(out["request-id"], "req_1");
        assert_eq!(out["x-rotate-account"], "acct-1");
    }

    #[test]
    fn upstream_message_prefers_json_error() {
        assert_eq!(
            upstream_message(br#"{"error":{"message":"overloaded"}}"#),
            "overloaded"
        );
        assert_eq!(upstream_message(b"plain failure"), "plain failure");
    }
}
