//! Journal d'audit (`logs/audit.jsonl`) et agrégats servis au panel.
//!
//! Chaque requête relayée produit une ligne JSON. Le panel n'affiche que les
//! 24 dernières heures : seule cette fenêtre est gardée en mémoire, ce qui évite
//! de relire un fichier qui ne fait que grossir. L'écriture disque passe par une
//! tâche dédiée pour que le chemin de requête ne bloque jamais sur une E/S.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::config::AlertConfig;
use crate::time;
use crate::usage::{Usage, api_cost};

/// Fenêtre glissante conservée en mémoire et agrégée par le panel.
const WINDOW_S: f64 = 86_400.0;
const HOUR_S: f64 = 3_600.0;
/// Un appareil vu depuis moins longtemps que ça est affiché « en ligne ».
const ONLINE_S: f64 = 600.0;
/// Valeur des champs absents, comme `rec.get(..., "?")` en Python.
const UNKNOWN: &str = "?";

/// Une ligne du journal d'audit, au format de l'implémentation Python.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditRecord {
    pub ts: String,
    pub device: String,
    pub account: String,
    pub path: String,
    pub model: Option<String>,
    pub status: u16,
    #[serde(skip_serializing_if = "is_zero")]
    pub retries: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// En-tête d'utilisation 5 h renvoyé par l'amont, tel quel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub util_5h: Option<String>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Message envoyé à la tâche d'écriture.
enum WriterMsg {
    /// Ligne JSON terminée par `\n`.
    Line(String),
    /// Écrire ce qui reste puis s'arrêter en acquittant.
    Shutdown(oneshot::Sender<()>),
}

/// Journal d'audit : fenêtre de 24 h en mémoire + écriture asynchrone sur disque.
pub struct AuditLog {
    /// `(horodatage Unix, enregistrement)`, dans l'ordre d'arrivée.
    records: Mutex<VecDeque<(f64, AuditRecord)>>,
    /// Absent pour un journal purement en mémoire.
    writer: Option<mpsc::UnboundedSender<WriterMsg>>,
}

impl AuditLog {
    /// Recharge les 24 dernières heures de `path` (s'il existe) puis lance la
    /// tâche d'écriture. Doit être appelé dans un runtime tokio.
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let records = match File::open(&path) {
            Ok(file) => load_window(BufReader::new(file), time::now() - WINDOW_S),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => VecDeque::new(),
            Err(err) => {
                return Err(
                    anyhow::Error::new(err).context(format!("lecture de {}", path.display()))
                );
            }
        };
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_writer(path, rx));
        Ok(Self {
            records: Mutex::new(records),
            writer: Some(tx),
        })
    }

    /// Journal sans fichier, pour les tests.
    pub fn in_memory() -> Self {
        Self {
            records: Mutex::new(VecDeque::new()),
            writer: None,
        }
    }

    /// Horodate et enregistre une requête. Synchrone et sans E/S : utilisable
    /// depuis un `Drop` (fin de flux SSE).
    pub fn record(&self, mut rec: AuditRecord) {
        rec.ts = time::local_timestamp();
        if let Some(writer) = &self.writer {
            match serde_json::to_string(&rec) {
                Ok(mut line) => {
                    line.push('\n');
                    // Échec seulement après `shutdown` : la ligne reste en mémoire.
                    if writer.send(WriterMsg::Line(line)).is_err() {
                        tracing::debug!("journal d'audit fermé, ligne non écrite");
                    }
                }
                Err(err) => tracing::warn!(%err, "sérialisation de l'audit impossible"),
            }
        }
        self.insert(time::now(), rec);
    }

    fn insert(&self, unix: f64, rec: AuditRecord) {
        let mut records = self.lock();
        purge(&mut records, unix - WINDOW_S);
        records.push_back((unix, rec));
    }

    /// Écrit les lignes en attente puis arrête la tâche d'écriture.
    pub async fn shutdown(&self) {
        let Some(writer) = &self.writer else { return };
        let (ack, done) = oneshot::channel();
        if writer.send(WriterMsg::Shutdown(ack)).is_ok() {
            // Une erreur signifie que la tâche est déjà terminée : rien à attendre.
            let _ = done.await;
        }
    }

    /// Agrégats des 24 dernières heures, au format de `aggregate_audit()`.
    pub fn stats(&self, ctx: &StatsContext<'_>, now: f64) -> Stats {
        let mut records = self.lock();
        purge(&mut records, now - WINDOW_S);
        aggregate(&records, ctx, now)
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<(f64, AuditRecord)>> {
        // Un panic ailleurs ne rend pas les enregistrements incohérents (ajouts
        // et retraits sont atomiques) : on continue avec les données en place.
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Retire les enregistrements plus anciens que `cutoff` (ordre chronologique).
fn purge(records: &mut VecDeque<(f64, AuditRecord)>, cutoff: f64) {
    while records.front().is_some_and(|(ts, _)| *ts < cutoff) {
        records.pop_front();
    }
}

/// Lit le fichier ligne à ligne ; lignes illisibles ou trop anciennes ignorées.
fn load_window(reader: impl BufRead, cutoff: f64) -> VecDeque<(f64, AuditRecord)> {
    let mut records = VecDeque::new();
    for line in reader.split(b'\n') {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                tracing::warn!(%err, "lecture du journal d'audit interrompue");
                break;
            }
        };
        let Ok(rec) = serde_json::from_slice::<AuditRecord>(&line) else {
            continue;
        };
        if let Some(ts) = time::parse_timestamp(&rec.ts).filter(|ts| *ts >= cutoff) {
            records.push_back((ts, rec));
        }
    }
    records
}

/// Tâche d'écriture : regroupe les lignes disponibles et les ajoute au fichier.
async fn run_writer(path: PathBuf, mut rx: mpsc::UnboundedReceiver<WriterMsg>) {
    while let Some(first) = rx.recv().await {
        let mut batch = String::new();
        let mut ack = None;
        let mut next = Some(first);
        while let Some(msg) = next {
            match msg {
                WriterMsg::Line(line) => batch.push_str(&line),
                WriterMsg::Shutdown(tx) => {
                    ack = Some(tx);
                    break;
                }
            }
            next = rx.try_recv().ok();
        }
        if !batch.is_empty() {
            let target = path.clone();
            match tokio::task::spawn_blocking(move || append(&target, batch.as_bytes())).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!(file = %path.display(), %err, "écriture de l'audit impossible");
                }
                Err(err) => tracing::warn!(%err, "tâche d'écriture de l'audit interrompue"),
            }
        }
        if let Some(ack) = ack {
            let _ = ack.send(());
            return;
        }
    }
}

/// Réouvre le fichier à chaque lot : une rotation externe (`mv audit.jsonl …`)
/// est prise en compte dès l'écriture suivante.
fn append(path: &Path, batch: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(batch)
}

/// Ce que l'agrégation doit connaître de la configuration et de l'état.
pub struct StatsContext<'a> {
    /// nom d'appareil -> clé.
    pub devices: &'a IndexMap<String, String>,
    pub prices: &'a IndexMap<String, [f64; 2]>,
    pub alerts: &'a AlertConfig,
    /// Seuil de bascule 5 h, rappelé dans le texte d'alerte.
    pub threshold: f64,
    /// `(compte, utilisation 5 h effective)`, dans l'ordre d'affichage.
    pub account_utils: Vec<(String, f64)>,
}

/// Réponse de `/rotate/stats`.
#[derive(Debug, Serialize)]
pub struct Stats {
    pub window: &'static str,
    pub devices: IndexMap<String, DeviceStatus>,
    pub by_device: BTreeMap<String, Bucket>,
    pub by_model: BTreeMap<String, Bucket>,
    pub by_account: BTreeMap<String, Bucket>,
    pub alerts: Vec<Alert>,
}

/// Consommation cumulée d'un appareil, d'un modèle ou d'un compte.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Bucket {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: f64,
}

impl Bucket {
    fn add(&mut self, usage: &Usage, cost: f64) {
        self.requests += 1;
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cache_read += usage.cache_read_input_tokens;
        self.cache_write += usage.cache_creation_input_tokens;
        self.cost_usd += cost;
    }
}

/// Ligne du tableau des appareils.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviceStatus {
    pub key_suffix: String,
    pub online: bool,
    pub last_seen_min: Option<i64>,
    pub last_model: Option<String>,
    pub last_account: Option<String>,
    pub req_1h: u64,
    pub tokens_1h: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertLevel {
    Warn,
    Info,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Alert {
    pub level: AlertLevel,
    pub text: String,
}

/// Dernière activité connue d'un appareil.
struct DeviceMeta<'r> {
    last_ts: f64,
    last_model: Option<&'r str>,
    last_account: &'r str,
}

#[derive(Default)]
struct HourActivity {
    requests: u64,
    tokens: u64,
}

fn or_unknown(s: &str) -> &str {
    if s.is_empty() { UNKNOWN } else { s }
}

fn aggregate(records: &VecDeque<(f64, AuditRecord)>, ctx: &StatsContext<'_>, now: f64) -> Stats {
    let hour_ago = now - HOUR_S;
    let mut by_device: BTreeMap<String, Bucket> = BTreeMap::new();
    let mut by_model: BTreeMap<String, Bucket> = BTreeMap::new();
    let mut by_account: BTreeMap<String, Bucket> = BTreeMap::new();
    let mut meta: HashMap<&str, DeviceMeta<'_>> = HashMap::new();
    let mut hour: BTreeMap<&str, HourActivity> = BTreeMap::new();
    let mut expensive_hour: BTreeMap<String, u64> = BTreeMap::new();

    for (ts, rec) in records {
        let ts = *ts;
        let usage = rec.usage.unwrap_or_default();
        let model = rec.model.as_deref().filter(|m| !m.is_empty());
        let cost = api_cost(ctx.prices, model, &usage);
        let device = or_unknown(&rec.device);

        let entry = meta.entry(device).or_insert(DeviceMeta {
            last_ts: 0.0,
            last_model: None,
            last_account: "",
        });
        if ts >= entry.last_ts {
            *entry = DeviceMeta {
                last_ts: ts,
                last_model: model,
                last_account: &rec.account,
            };
        }

        by_device
            .entry(device.to_owned())
            .or_default()
            .add(&usage, cost);
        by_model
            .entry(model.unwrap_or(UNKNOWN).to_owned())
            .or_default()
            .add(&usage, cost);
        by_account
            .entry(or_unknown(&rec.account).to_owned())
            .or_default()
            .add(&usage, cost);

        if ts >= hour_ago {
            let activity = hour.entry(device).or_default();
            activity.requests += 1;
            activity.tokens += usage.billable_tokens();
            let model = model.unwrap_or("");
            if ctx
                .alerts
                .expensive_model_patterns
                .iter()
                .any(|p| model.contains(p.as_str()))
            {
                *expensive_hour
                    .entry(format!("{device} / {model}"))
                    .or_default() += 1;
            }
        }
    }

    let mut alerts = Vec::new();
    let limit = ctx.alerts.device_tokens_per_hour;
    for (device, activity) in &hour {
        if activity.tokens > limit {
            alerts.push(Alert {
                level: AlertLevel::Warn,
                text: format!(
                    "{device} consumed {} tokens in the last hour (threshold {})",
                    thousands(activity.tokens),
                    thousands(limit)
                ),
            });
        }
    }
    for (key, n) in &expensive_hour {
        alerts.push(Alert {
            level: AlertLevel::Info,
            text: format!("expensive model in use: {key} — {n} requests in the last hour"),
        });
    }
    for (name, util) in &ctx.account_utils {
        if *util >= ctx.alerts.util_warn {
            alerts.push(Alert {
                level: AlertLevel::Warn,
                text: format!(
                    "{name} 5h window at {} (switch threshold {})",
                    percent(*util),
                    percent(ctx.threshold)
                ),
            });
        }
    }

    // Le tableau couvre chaque appareil déclaré, qu'il ait du trafic ou non.
    let devices = ctx
        .devices
        .iter()
        .map(|(name, key)| {
            let seen = meta.get(name.as_str());
            let activity = hour.get(name.as_str());
            let status = DeviceStatus {
                key_suffix: key_suffix(key),
                online: seen.is_some_and(|m| now - m.last_ts < ONLINE_S),
                last_seen_min: seen.map(|m| ((now - m.last_ts) / 60.0).round_ties_even() as i64),
                last_model: seen.and_then(|m| m.last_model).map(str::to_owned),
                last_account: seen
                    .map(|m| m.last_account)
                    .filter(|a| !a.is_empty())
                    .map(str::to_owned),
                req_1h: activity.map_or(0, |a| a.requests),
                tokens_1h: activity.map_or(0, |a| a.tokens),
            };
            (name.clone(), status)
        })
        .collect();

    Stats {
        window: "last 24h",
        devices,
        by_device,
        by_model,
        by_account,
        alerts,
    }
}

/// `…` + les 4 derniers caractères de la clé (jamais la clé entière).
fn key_suffix(key: &str) -> String {
    let start = key.char_indices().rev().nth(3).map_or(0, |(i, _)| i);
    format!("…{}", &key[start..])
}

/// Équivalent de `f"{n:,}"` en Python.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Équivalent de `f"{x:.0%}"` en Python.
fn percent(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_prices;

    const NOW: f64 = 1_793_000_000.0;

    fn rec(device: &str, model: &str, account: &str, inp: u64, out: u64) -> AuditRecord {
        AuditRecord {
            device: device.into(),
            account: account.into(),
            path: "/v1/messages".into(),
            model: Some(model.into()),
            status: 200,
            usage: Some(Usage {
                input_tokens: inp,
                output_tokens: out,
                ..Usage::default()
            }),
            ..AuditRecord::default()
        }
    }

    fn devices() -> IndexMap<String, String> {
        [
            ("laptop", "key-laptop-abcd"),
            ("ci-box", "key-ci-efgh"),
            ("idle-dev", "key-idle-ijkl"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
    }

    /// Jeu de données du test Python `test_analytics.py`, dans l'ordre chronologique.
    fn sample() -> AuditLog {
        let log = AuditLog::in_memory();
        log.insert(
            NOW - 90_000.0,
            rec("laptop", "claude-opus-5", "a", 9_999, 9_999),
        );
        log.insert(
            NOW - 7_200.0,
            rec("laptop", "claude-haiku-4-5", "a", 2_000, 100),
        );
        log.insert(
            NOW - 120.0,
            rec("ci-box", "claude-sonnet-5", "b", 6_000_000, 10),
        );
        log.insert(NOW - 60.0, rec("laptop", "claude-opus-5", "a", 1_000, 500));
        log
    }

    fn stats_of(log: &AuditLog, utils: Vec<(String, f64)>) -> Stats {
        let (devices, prices, alerts) = (devices(), default_prices(), AlertConfig::default());
        let ctx = StatsContext {
            devices: &devices,
            prices: &prices,
            alerts: &alerts,
            threshold: 0.8,
            account_utils: utils,
        };
        log.stats(&ctx, NOW)
    }

    fn alert_texts(stats: &Stats) -> String {
        stats
            .alerts
            .iter()
            .map(|a| a.text.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn rollups_by_device_model_and_account() {
        let stats = stats_of(&sample(), Vec::new());
        let laptop = &stats.by_device["laptop"];
        assert_eq!((laptop.requests, laptop.input_tokens), (2, 3_000));
        assert_eq!(stats.by_model["claude-opus-5"].requests, 1);
        assert_eq!(stats.by_account["b"].requests, 1);
        assert_eq!(stats.by_account["a"].requests, 2);
    }

    #[test]
    fn cost_is_accumulated_at_api_prices() {
        let stats = stats_of(&sample(), Vec::new());
        // opus : 1000 × 5 + 500 × 25 ; haiku : 2000 × 1 + 100 × 5 (par million).
        let expected = (1_000.0 * 5.0 + 500.0 * 25.0 + 2_000.0 + 100.0 * 5.0) / 1e6;
        assert!((stats.by_device["laptop"].cost_usd - expected).abs() < 1e-12);
    }

    #[test]
    fn records_older_than_a_day_are_purged() {
        let log = AuditLog::in_memory();
        log.insert(
            NOW - 90_000.0,
            rec("laptop", "claude-opus-5", "a", 9_999, 9_999),
        );
        log.insert(
            NOW - 7_200.0,
            rec("laptop", "claude-haiku-4-5", "a", 2_000, 100),
        );
        assert_eq!(log.len(), 2, "encore dans la fenêtre du dernier ajout");
        let stats = stats_of(&log, Vec::new());
        assert_eq!(log.len(), 1);
        assert_eq!(stats.by_device["laptop"].output_tokens, 100);

        // L'insertion purge aussi, relativement à l'horodatage ajouté.
        log.insert(NOW + 86_000.0, rec("laptop", "claude-opus-5", "a", 1, 1));
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn alerts_match_python_texts() {
        let stats = stats_of(&sample(), vec![("a".into(), 0.85), ("b".into(), 0.1)]);
        let texts = alert_texts(&stats);
        assert!(
            texts.contains(
                "ci-box consumed 6,000,010 tokens in the last hour (threshold 5,000,000)"
            ),
            "{texts}"
        );
        assert!(
            texts.contains("expensive model in use: laptop / claude-opus-5 — 1 requests"),
            "{texts}"
        );
        assert!(
            texts.contains("a 5h window at 85% (switch threshold 80%)"),
            "{texts}"
        );
        assert!(!texts.contains("b 5h"), "{texts}");
        assert_eq!(stats.alerts.len(), 3);
        assert_eq!(stats.alerts[1].level, AlertLevel::Info);
    }

    #[test]
    fn roster_lists_every_device_in_config_order() {
        let stats = stats_of(&sample(), Vec::new());
        let names: Vec<&str> = stats.devices.keys().map(String::as_str).collect();
        assert_eq!(names, ["laptop", "ci-box", "idle-dev"]);

        let laptop = &stats.devices["laptop"];
        assert!(laptop.online);
        assert_eq!(laptop.last_seen_min, Some(1));
        assert_eq!(laptop.key_suffix, "…abcd");
        assert_eq!(laptop.last_model.as_deref(), Some("claude-opus-5"));
        assert_eq!(laptop.last_account.as_deref(), Some("a"));

        let ci = &stats.devices["ci-box"];
        assert_eq!((ci.req_1h, ci.tokens_1h), (1, 6_000_010));

        let idle = &stats.devices["idle-dev"];
        assert!(!idle.online);
        assert_eq!(idle.last_seen_min, None);
        assert_eq!((idle.req_1h, idle.tokens_1h), (0, 0));
    }

    #[test]
    fn stats_serialize_like_python() {
        let json = serde_json::to_value(stats_of(&sample(), Vec::new())).unwrap();
        assert_eq!(json["window"], "last 24h");
        assert_eq!(
            json["devices"]["idle-dev"]["last_seen_min"],
            serde_json::Value::Null
        );
        assert_eq!(json["by_device"]["laptop"]["cache_read"], 0);
        assert_eq!(json["alerts"][0]["level"], "warn");
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(6_000_010), "6,000,010");
        assert_eq!(percent(0.85), "85%");
        assert_eq!(percent(1.0), "100%");
        assert_eq!(key_suffix("ab"), "…ab");
        assert_eq!(key_suffix("clé-xéèàù"), "…éèàù");
    }

    #[test]
    fn python_audit_line_is_readable() {
        let line = r#"{"device":"laptop","account":"a","path":"/v1/messages","model":"claude-opus-5","status":200,"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":200,"cache_creation_input_tokens":0},"util_5h":"0.5","ts":"2026-11-08T10:00:00+0100"}"#;
        let rec: AuditRecord = serde_json::from_str(line).unwrap();
        assert_eq!(rec.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(rec.usage.map(|u| u.cache_read_input_tokens), Some(200));
        assert_eq!(rec.util_5h.as_deref(), Some("0.5"));
        assert_eq!(rec.retries, 0);
        assert!(time::parse_timestamp(&rec.ts).is_some());
    }

    #[test]
    fn record_serialization_omits_empty_fields() {
        let json = serde_json::to_value(AuditRecord::default()).unwrap();
        let obj = json.as_object().unwrap();
        assert!(!obj.contains_key("retries"));
        assert!(!obj.contains_key("usage"));
        assert!(!obj.contains_key("util_5h"));
    }

    fn line_at(ts: f64, device: &str) -> String {
        let mut r = rec(device, "claude-sonnet-5", "a", 1, 1);
        r.ts = chrono::DateTime::from_timestamp(ts as i64, 0)
            .unwrap()
            .format(time::TS_FORMAT)
            .to_string();
        serde_json::to_string(&r).unwrap()
    }

    #[tokio::test]
    async fn open_reloads_recent_lines_and_skips_corrupt_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let now = time::now();
        let content = [
            line_at(now - 90_000.0, "old"),
            "not json".to_owned(),
            r#"{"device":"no-ts"}"#.to_owned(),
            line_at(now - 60.0, "laptop"),
            "{\"device\":\"trunc".to_owned(),
        ]
        .join("\n");
        fs::write(&path, content).unwrap();

        let log = AuditLog::open(path).unwrap();
        assert_eq!(log.len(), 1);
        log.shutdown().await;
    }

    #[tokio::test]
    async fn record_then_shutdown_writes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("audit.jsonl");
        let log = AuditLog::open(path.clone()).unwrap();
        assert!(log.is_empty());
        log.record(rec("laptop", "claude-opus-5", "a", 1, 2));
        log.record(rec("ci-box", "claude-haiku-5", "b", 3, 4));
        log.shutdown().await;

        let written = fs::read_to_string(&path).unwrap();
        let lines: Vec<AuditRecord> = written
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].device, "ci-box");
        assert!(time::parse_timestamp(&lines[0].ts).is_some());
        assert_eq!(log.len(), 2);

        // Après l'arrêt, la ligne reste comptée en mémoire sans paniquer.
        log.record(rec("laptop", "claude-opus-5", "a", 1, 1));
        assert_eq!(log.len(), 3);
    }
}
