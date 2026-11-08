//! Politique de rotation des comptes.
//!
//! Tout est pur et synchrone : l'horloge est passée en paramètre (`now`, en
//! secondes Unix), ce qui rend chaque décision reproductible en test. Le serveur
//! garde un unique [`Rotator`] derrière un mutex : c'est ce verrou qui garantit
//! qu'une salve de requêtes frappant un quota ne provoque qu'une seule bascule.

use http::HeaderMap;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::config::{Account, Config, Strategy};
use crate::time::local_timestamp;

/// Nombre maximal d'événements conservés dans `state.json`.
pub const MAX_EVENTS: usize = 200;
/// Pause maximale appliquée sur un 429 de rafale.
const MAX_PACE_S: f64 = 60.0;
/// Pause par défaut quand `retry-after` est absent ou illisible.
const DEFAULT_PACE_S: f64 = 15.0;
/// Délai supposé avant un soulagement quand aucune réinitialisation n'est connue.
const UNKNOWN_RELIEF_S: f64 = 300.0;

/// En-têtes de télémétrie de quota renvoyés par Anthropic.
pub mod headers {
    pub const UTIL_5H: &str = "anthropic-ratelimit-unified-5h-utilization";
    pub const UTIL_7D: &str = "anthropic-ratelimit-unified-7d-utilization";
    pub const RESET_5H: &str = "anthropic-ratelimit-unified-5h-reset";
    pub const RESET_7D: &str = "anthropic-ratelimit-unified-7d-reset";
    pub const STATUS: &str = "anthropic-ratelimit-unified-status";
    pub const RETRY_AFTER: &str = "retry-after";
}

/// Télémétrie connue pour un compte (format de `state.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub util_5h: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub util_7d: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_5h: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_7d: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pace_until: Option<f64>,
}

/// Bascule ou mise en attente, telle qu'affichée dans le panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    #[serde(default)]
    pub ts: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub util: Option<f64>,
}

/// État persistant complet (`state.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub active_account: String,
    pub accounts: IndexMap<String, AccountState>,
    pub events: Vec<Event>,
    pub last_switch_ts: f64,
}

/// Ce que [`Rotator::note_response`] conclut d'un 429 ou d'une réponse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 429 de rafale (par minute) : même compte, on temporise.
    Burst,
    /// Le compte actif a changé : une nouvelle tentative partira sur le nouveau.
    Switched,
    /// Quota épuisé et aucun meilleur compte disponible.
    Exhausted,
}

/// Instantané des en-têtes de quota d'une réponse amont.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RateHeaders {
    pub util_5h: Option<f64>,
    pub util_7d: Option<f64>,
    pub reset_5h: Option<f64>,
    pub reset_7d: Option<f64>,
    pub status: Option<String>,
    pub retry_after: Option<String>,
}

impl RateHeaders {
    pub fn from_headers(map: &HeaderMap) -> Self {
        let text = |name: &str| {
            map.get(name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let number = |name: &str| text(name).and_then(|s| s.parse::<f64>().ok());
        Self {
            util_5h: number(headers::UTIL_5H),
            util_7d: number(headers::UTIL_7D),
            reset_5h: number(headers::RESET_5H),
            reset_7d: number(headers::RESET_7D),
            status: text(headers::STATUS),
            retry_after: text(headers::RETRY_AFTER),
        }
    }
}

/// Réglages de la politique, extraits de la configuration.
#[derive(Debug, Clone)]
pub struct Policy {
    pub threshold: f64,
    pub threshold_7d: f64,
    pub strategy: Strategy,
    pub cooldown_s: f64,
    pub switch_margin: f64,
    pub consume_first_margin_s: f64,
}

impl Default for Policy {
    fn default() -> Self {
        Self::from(&Config::default())
    }
}

impl From<&Config> for Policy {
    fn from(cfg: &Config) -> Self {
        Self {
            threshold: cfg.switch_threshold,
            threshold_7d: cfg.switch_threshold_7d,
            strategy: cfg.strategy,
            cooldown_s: cfg.switch_cooldown_s,
            switch_margin: cfg.switch_margin,
            consume_first_margin_s: cfg.consume_first_margin_s,
        }
    }
}

/// Ce que la rotation doit savoir d'un compte (jamais son jeton).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountMeta {
    pub name: String,
    pub priority: i64,
    pub disabled: bool,
}

impl From<&Account> for AccountMeta {
    fn from(a: &Account) -> Self {
        Self { name: a.name.clone(), priority: a.priority, disabled: a.disabled }
    }
}

/// Moteur de rotation : politique + comptes configurés + état courant.
#[derive(Debug, Clone)]
pub struct Rotator {
    policy: Policy,
    accounts: Vec<AccountMeta>,
    state: State,
}

impl Rotator {
    /// `accounts` ne doit pas être vide. Un compte actif inconnu (retiré de la
    /// configuration depuis la dernière exécution) est remplacé par le premier.
    pub fn new(policy: Policy, accounts: Vec<AccountMeta>, mut state: State) -> Self {
        assert!(!accounts.is_empty(), "au moins un compte est requis");
        if !accounts.iter().any(|a| a.name == state.active_account) {
            state.active_account = accounts[0].name.clone();
        }
        Self { policy, accounts, state }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn accounts(&self) -> &[AccountMeta] {
        &self.accounts
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn active(&self) -> &str {
        &self.state.active_account
    }

    pub fn account_state(&self, name: &str) -> Option<&AccountState> {
        self.state.accounts.get(name)
    }

    fn meta(&self, name: &str) -> Option<&AccountMeta> {
        self.accounts.iter().find(|a| a.name == name)
    }

    /// Plus bas = préféré.
    pub fn priority(&self, name: &str) -> i64 {
        self.meta(name).map_or(i64::MAX, |m| m.priority)
    }

    pub fn disabled(&self, name: &str) -> bool {
        self.meta(name).is_none_or(|m| m.disabled)
    }

    /// Utilisation effective de la fenêtre de 5 h : une fenêtre dont la
    /// réinitialisation est passée ne compte plus.
    pub fn util_5h(&self, name: &str, now: f64) -> f64 {
        let info = self.account_state(name);
        window_util(info.and_then(|i| i.util_5h), info.and_then(|i| i.reset_5h), now)
    }

    /// Utilisation effective de la fenêtre hebdomadaire.
    pub fn util_7d(&self, name: &str, now: f64) -> f64 {
        let info = self.account_state(name);
        window_util(info.and_then(|i| i.util_7d), info.and_then(|i| i.reset_7d), now)
    }

    /// Utilisable = actif et sous les deux seuils.
    pub fn usable(&self, name: &str, now: f64) -> bool {
        !self.disabled(name)
            && self.util_5h(name, now) < self.policy.threshold
            && self.util_7d(name, now) < self.policy.threshold_7d
    }

    fn weekly_reset(&self, name: &str) -> f64 {
        self.account_state(name)
            .and_then(|i| i.reset_7d)
            .filter(|r| *r != 0.0)
            .unwrap_or(f64::INFINITY)
    }

    fn reset_5h_or_zero(&self, name: &str) -> f64 {
        self.account_state(name).and_then(|i| i.reset_5h).unwrap_or(0.0)
    }

    /// Prochaine réinitialisation de fenêtre de 5 h, tous comptes confondus.
    pub fn earliest_relief(&self, now: f64) -> f64 {
        self.accounts
            .iter()
            .filter_map(|a| self.account_state(&a.name)?.reset_5h)
            .filter(|r| *r > now)
            .min_by(f64::total_cmp)
            .unwrap_or(now + UNKNOWN_RELIEF_S)
    }

    /// Temps de pause restant sur un compte après un 429 de rafale.
    pub fn pace_remaining(&self, name: &str, now: f64) -> f64 {
        self.account_state(name)
            .and_then(|i| i.pace_until)
            .map_or(0.0, |until| (until - now).max(0.0))
    }

    /// Choisit le meilleur compte.
    ///
    /// Les paliers de priorité passent d'abord ; la stratégie ordonne les
    /// comptes à l'intérieur d'un palier. Si aucun compte n'est utilisable, on
    /// prend le compte actif dont la fenêtre de 5 h se réinitialise le plus tôt.
    pub fn pick_account(&self, now: f64) -> &str {
        let usable = self.accounts.iter().filter(|a| self.usable(&a.name, now));
        let best = match self.policy.strategy {
            Strategy::ConsumeFirst => usable.min_by(|a, b| {
                a.priority
                    .cmp(&b.priority)
                    .then_with(|| self.weekly_reset(&a.name).total_cmp(&self.weekly_reset(&b.name)))
                    .then_with(|| {
                        self.util_5h(&a.name, now).total_cmp(&self.util_5h(&b.name, now))
                    })
            }),
            Strategy::LeastUsed => usable.min_by(|a, b| {
                a.priority.cmp(&b.priority).then_with(|| {
                    self.util_5h(&a.name, now).total_cmp(&self.util_5h(&b.name, now))
                })
            }),
        };
        if let Some(best) = best {
            return &best.name;
        }

        let enabled: Vec<&AccountMeta> = self.accounts.iter().filter(|a| !a.disabled).collect();
        let pool = if enabled.is_empty() { self.accounts.iter().collect() } else { enabled };
        pool.into_iter()
            .min_by(|a, b| self.reset_5h_or_zero(&a.name).total_cmp(&self.reset_5h_or_zero(&b.name)))
            .map_or(&self.accounts[0].name, |a| &a.name)
    }

    /// Intègre la télémétrie d'une réponse amont et bascule si nécessaire.
    ///
    /// Renvoie `None` quand rien de notable ne s'est produit.
    pub fn note_response(
        &mut self,
        account: &str,
        rate: &RateHeaders,
        status: u16,
        now: f64,
    ) -> Option<Verdict> {
        let info = self.state.accounts.entry(account.to_owned()).or_default();
        merge(&mut info.util_5h, rate.util_5h);
        merge(&mut info.util_7d, rate.util_7d);
        merge(&mut info.reset_5h, rate.reset_5h);
        merge(&mut info.reset_7d, rate.reset_7d);
        if rate.status.is_some() {
            info.status.clone_from(&rate.status);
        }
        info.last_seen = Some(now);
        let rejected = info.status.as_deref() == Some("rejected");

        let quota_hit = rejected
            || (status == 429
                && (self.util_5h(account, now) >= 1.0 || self.util_7d(account, now) >= 1.0));

        if status == 429 && !quota_hit {
            // Limite par minute, pas un quota épuisé : basculer déplacerait la
            // rafale sur le compte suivant et jetterait son cache de prompt.
            let pace = rate
                .retry_after
                .as_deref()
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|p| p.is_finite() && *p >= 0.0)
                .map_or(DEFAULT_PACE_S, |p| p.min(MAX_PACE_S));
            self.state.accounts.entry(account.to_owned()).or_default().pace_until =
                Some(now + pace);
            return Some(Verdict::Burst);
        }

        if self.state.active_account != account {
            // Une autre requête a déjà basculé : la nouvelle tentative suivra.
            return quota_hit.then_some(Verdict::Switched);
        }

        let reason = self.switch_reason(account, quota_hit, now);
        match reason {
            Some((next, reason)) => {
                let util = self.util_5h(account, now);
                self.state.active_account.clone_from(&next);
                self.state.last_switch_ts = now;
                self.push_event(Event {
                    ts: local_timestamp(),
                    kind: "switch".into(),
                    from: account.to_owned(),
                    to: next,
                    reason,
                    util: Some(util),
                });
                Some(Verdict::Switched)
            }
            None if quota_hit => Some(Verdict::Exhausted),
            None => None,
        }
    }

    /// Décide s'il faut quitter `account` (le compte actif), et pour lequel.
    fn switch_reason(&self, account: &str, quota_hit: bool, now: f64) -> Option<(String, String)> {
        let next = self.pick_account(now);
        if next == account {
            return None;
        }
        let p = &self.policy;
        let cooled = now - self.state.last_switch_ts >= p.cooldown_s;
        let week_spent = self.util_7d(account, now) >= p.threshold_7d;
        let over = self.util_5h(account, now) >= p.threshold || week_spent;

        let reason = if quota_hit {
            // Arrêt dur : ni délai de grâce ni marge.
            Some("quota_exhausted".to_owned())
        } else if self.disabled(account) {
            Some("account_disabled".to_owned())
        } else if over && cooled {
            // Hystérésis : ne bouger que vers un compte nettement meilleur, pour
            // que deux comptes au bord du seuil ne fassent pas de ping-pong.
            (week_spent
                || self.priority(next) < self.priority(account)
                || self.util_5h(next, now) <= self.util_5h(account, now) - p.switch_margin)
                .then(|| format!("utilization>={}", p.threshold))
        } else if cooled && self.usable(next, now) {
            if self.priority(next) < self.priority(account) {
                // Un compte préféré a de nouveau de la marge : on y revient.
                Some("priority_recovery".to_owned())
            } else if p.strategy == Strategy::ConsumeFirst
                && self.weekly_reset(next) < self.weekly_reset(account) - p.consume_first_margin_s
            {
                // Quota hebdomadaire périssable : consommer d'abord celui qui expire.
                Some("consume_first".to_owned())
            } else {
                None
            }
        } else {
            None
        };
        reason.map(|r| (next.to_owned(), r))
    }

    /// Ajoute un événement en bornant l'historique à [`MAX_EVENTS`].
    pub fn push_event(&mut self, event: Event) {
        self.state.events.push(event);
        let excess = self.state.events.len().saturating_sub(MAX_EVENTS);
        if excess > 0 {
            self.state.events.drain(..excess);
        }
    }

    /// Les `n` derniers événements, du plus ancien au plus récent.
    pub fn recent_events(&self, n: usize) -> &[Event] {
        let events = &self.state.events;
        &events[events.len().saturating_sub(n)..]
    }
}

fn merge(slot: &mut Option<f64>, value: Option<f64>) {
    if value.is_some() {
        *slot = value;
    }
}

fn window_util(util: Option<f64>, reset: Option<f64>, now: f64) -> f64 {
    match reset {
        Some(r) if r != 0.0 && now >= r => 0.0,
        _ => util.unwrap_or(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_800_000_000.0;

    fn meta(name: &str, priority: i64, disabled: bool) -> AccountMeta {
        AccountMeta { name: name.into(), priority, disabled }
    }

    fn abc() -> Vec<AccountMeta> {
        vec![meta("a", 100, false), meta("b", 100, false), meta("c", 100, false)]
    }

    fn acct(u5: f64, u7: f64) -> AccountState {
        AccountState { util_5h: Some(u5), util_7d: Some(u7), ..Default::default() }
    }

    fn rotator(accounts: Vec<AccountMeta>, active: &str, data: &[(&str, AccountState)]) -> Rotator {
        let state = State {
            active_account: active.into(),
            accounts: data.iter().map(|(n, s)| ((*n).to_owned(), s.clone())).collect(),
            ..Default::default()
        };
        Rotator::new(Policy::default(), accounts, state)
    }

    fn rate(u5: f64) -> RateHeaders {
        RateHeaders { util_5h: Some(u5), ..Default::default() }
    }

    fn rejected(u5: f64) -> RateHeaders {
        RateHeaders { util_5h: Some(u5), status: Some("rejected".into()), ..Default::default() }
    }

    fn last_reason(r: &Rotator) -> &str {
        &r.state().events.last().expect("un événement").reason
    }

    #[test]
    fn consume_first_picks_soonest_weekly_reset() {
        let mut r = rotator(
            abc(),
            "a",
            &[
                ("a", AccountState { reset_7d: Some(NOW + 500_000.0), ..acct(0.1, 0.3) }),
                ("b", AccountState { reset_7d: Some(NOW + 100_000.0), ..acct(0.5, 0.6) }),
                ("c", AccountState { reset_7d: Some(NOW + 300_000.0), ..acct(0.05, 0.1) }),
            ],
        );
        assert_eq!(r.pick_account(NOW), "b");

        r.state.accounts["b"].util_5h = Some(0.9);
        assert_eq!(r.pick_account(NOW), "c", "un compte au-dessus du seuil 5 h est ignoré");

        r.policy.strategy = Strategy::LeastUsed;
        assert_eq!(r.pick_account(NOW), "c");
    }

    #[test]
    fn burst_429_paces_without_rotating() {
        let mut r = rotator(abc(), "a", &[("a", acct(0.4, 0.2))]);
        let h = RateHeaders { retry_after: Some("7".into()), ..rate(0.4) };
        assert_eq!(r.note_response("a", &h, 429, NOW), Some(Verdict::Burst));
        assert_eq!(r.active(), "a");
        assert_eq!(r.pace_remaining("a", NOW), 7.0);
    }

    #[test]
    fn burst_pace_is_capped_and_defaults() {
        let mut r = rotator(abc(), "a", &[]);
        let h = RateHeaders { retry_after: Some("600".into()), ..rate(0.1) };
        r.note_response("a", &h, 429, NOW);
        assert_eq!(r.pace_remaining("a", NOW), MAX_PACE_S);
        r.note_response("a", &rate(0.1), 429, NOW);
        assert_eq!(r.pace_remaining("a", NOW), DEFAULT_PACE_S);
    }

    #[test]
    fn quota_429_switches_even_during_cooldown() {
        let mut r = rotator(
            abc(),
            "a",
            &[
                ("a", AccountState { reset_7d: Some(NOW + 500_000.0), ..acct(1.0, 0.5) }),
                ("b", AccountState { reset_7d: Some(NOW + 100_000.0), ..acct(0.1, 0.2) }),
                ("c", AccountState { reset_7d: Some(NOW + 300_000.0), ..acct(0.1, 0.2) }),
            ],
        );
        r.state.last_switch_ts = NOW;
        assert_eq!(r.note_response("a", &rejected(1.0), 429, NOW), Some(Verdict::Switched));
        assert_eq!(r.active(), "b");
        assert_eq!(last_reason(&r), "quota_exhausted");
    }

    #[test]
    fn all_spent_moves_toward_relief_then_exhausts() {
        let mut r = rotator(
            abc(),
            "a",
            &[
                ("a", AccountState { reset_5h: Some(NOW + 1000.0), ..acct(1.0, 0.0) }),
                ("b", AccountState { reset_5h: Some(NOW + 2000.0), ..acct(0.95, 0.0) }),
                ("c", AccountState { reset_5h: Some(NOW + 500.0), ..acct(0.9, 0.0) }),
            ],
        );
        assert_eq!(r.note_response("a", &rejected(1.0), 429, NOW), Some(Verdict::Switched));
        assert_eq!(r.active(), "c");
        assert_eq!(r.note_response("c", &rejected(0.9), 429, NOW), Some(Verdict::Exhausted));
        assert_eq!(r.earliest_relief(NOW), NOW + 500.0);
    }

    #[test]
    fn stale_switch_by_another_request_is_reported() {
        let mut r = rotator(abc(), "b", &[]);
        assert_eq!(r.note_response("a", &rejected(1.0), 429, NOW), Some(Verdict::Switched));
        let allowed = RateHeaders { status: Some("allowed".into()), ..rate(0.5) };
        assert_eq!(r.note_response("a", &allowed, 200, NOW), None);
        assert!(r.state().events.is_empty());
    }

    #[test]
    fn threshold_switch_respects_cooldown() {
        let mut r =
            rotator(abc(), "a", &[("a", acct(0.85, 0.0)), ("b", acct(0.1, 0.0)), ("c", acct(0.2, 0.0))]);
        r.state.last_switch_ts = NOW;
        assert_eq!(r.note_response("a", &rate(0.85), 200, NOW), None);
        assert_eq!(r.active(), "a");

        r.state.last_switch_ts = 0.0;
        assert_eq!(r.note_response("a", &rate(0.85), 200, NOW), Some(Verdict::Switched));
        assert_eq!(r.active(), "b");
        assert_eq!(last_reason(&r), "utilization>=0.8");
    }

    #[test]
    fn hysteresis_prevents_ping_pong() {
        let mut r =
            rotator(abc(), "a", &[("a", acct(0.81, 0.0)), ("b", acct(0.79, 0.0)), ("c", acct(0.80, 0.0))]);
        assert_eq!(r.note_response("a", &rate(0.81), 200, NOW), None);
        assert_eq!(r.active(), "a");
    }

    #[test]
    fn proactive_consume_first_switch() {
        let reset = |s: f64| AccountState { reset_7d: Some(NOW + s), ..acct(0.3, 0.4) };
        let mut r =
            rotator(abc(), "a", &[("a", reset(500_000.0)), ("b", reset(100_000.0)), ("c", reset(300_000.0))]);
        assert_eq!(r.note_response("a", &rate(0.3), 200, NOW), Some(Verdict::Switched));
        assert_eq!(r.active(), "b");
        assert_eq!(last_reason(&r), "consume_first");

        let mut r = rotator(
            vec![meta("a", 100, false), meta("b", 100, false)],
            "a",
            &[("a", reset(101_000.0)), ("b", reset(100_000.0))],
        );
        assert_eq!(r.note_response("a", &rate(0.3), 200, NOW), None, "marge consume-first");
    }

    #[test]
    fn passed_resets_count_as_zero() {
        let r = rotator(
            abc(),
            "a",
            &[(
                "a",
                AccountState {
                    reset_5h: Some(NOW - 10.0),
                    reset_7d: Some(NOW - 10.0),
                    ..acct(1.0, 0.9)
                },
            )],
        );
        assert_eq!(r.util_5h("a", NOW), 0.0);
        assert_eq!(r.util_7d("a", NOW), 0.0);
    }

    #[test]
    fn priority_tiers_beat_strategy() {
        let tiers = vec![meta("a", 2, false), meta("b", 1, false), meta("c", 1, false)];
        let reset = |s: f64| AccountState { reset_7d: Some(NOW + s), ..acct(0.1, 0.1) };
        let mut r =
            rotator(tiers, "a", &[("a", reset(1000.0)), ("b", reset(500_000.0)), ("c", reset(100_000.0))]);
        assert_eq!(r.pick_account(NOW), "c");

        r.state.accounts["b"].util_5h = Some(0.9);
        r.state.accounts["c"].util_5h = Some(0.9);
        assert_eq!(r.pick_account(NOW), "a", "palier de secours une fois le préféré épuisé");
    }

    #[test]
    fn priority_recovery_pulls_traffic_back_after_cooldown() {
        let tiers = || vec![meta("a", 2, false), meta("b", 1, false), meta("c", 1, false)];
        let data = [("a", acct(0.2, 0.1)), ("b", acct(0.1, 0.1)), ("c", acct(0.9, 0.1))];

        let mut r = rotator(tiers(), "a", &data);
        assert_eq!(r.note_response("a", &rate(0.2), 200, NOW), Some(Verdict::Switched));
        assert_eq!(r.active(), "b");
        assert_eq!(last_reason(&r), "priority_recovery");

        let mut r = rotator(tiers(), "a", &data);
        r.state.last_switch_ts = NOW;
        assert_eq!(r.note_response("a", &rate(0.2), 200, NOW), None);
    }

    #[test]
    fn disabled_accounts_are_never_picked_and_left_at_once() {
        let accounts = || vec![meta("a", 100, true), meta("b", 100, false), meta("c", 100, false)];
        let mut r =
            rotator(accounts(), "a", &[("a", acct(0.0, 0.0)), ("b", acct(0.2, 0.0)), ("c", acct(0.1, 0.0))]);
        assert_eq!(r.pick_account(NOW), "c");
        r.state.last_switch_ts = NOW;
        assert_eq!(r.note_response("a", &rate(0.0), 200, NOW), Some(Verdict::Switched));
        assert_eq!(r.active(), "c");
        assert_eq!(last_reason(&r), "account_disabled");

        let spent = |reset: f64| AccountState { reset_5h: Some(NOW + reset), ..acct(1.0, 0.0) };
        let r = rotator(accounts(), "b", &[("a", spent(10.0)), ("b", spent(100.0)), ("c", spent(50.0))]);
        assert_eq!(r.pick_account(NOW), "c", "le repli ignore aussi les comptes désactivés");
    }

    #[test]
    fn unknown_active_account_falls_back_to_first() {
        let r = rotator(abc(), "removed", &[]);
        assert_eq!(r.active(), "a");
    }

    #[test]
    fn events_are_bounded() {
        let mut r = rotator(abc(), "a", &[]);
        for _ in 0..(MAX_EVENTS + 50) {
            r.push_event(Event {
                ts: String::new(),
                kind: "switch".into(),
                from: "a".into(),
                to: "b".into(),
                reason: "test".into(),
                util: None,
            });
        }
        assert_eq!(r.state().events.len(), MAX_EVENTS);
        assert_eq!(r.recent_events(10).len(), 10);
    }

    #[test]
    fn rate_headers_parse_and_ignore_garbage() {
        let mut map = HeaderMap::new();
        map.insert(headers::UTIL_5H, "0.42".parse().unwrap());
        map.insert(headers::UTIL_7D, "pas-un-nombre".parse().unwrap());
        map.insert(headers::STATUS, "allowed".parse().unwrap());
        let h = RateHeaders::from_headers(&map);
        assert_eq!(h.util_5h, Some(0.42));
        assert_eq!(h.util_7d, None);
        assert_eq!(h.status.as_deref(), Some("allowed"));
    }

    #[test]
    fn state_json_is_compatible_with_python_version() {
        let raw = r#"{"active_account":"a","accounts":{"a":{"util_5h":0.5,"reset_5h":1.0,
            "status":"allowed","last_seen":2.0}},"events":[{"ts":"x","type":"switch",
            "from":"a","to":"b","reason":"consume_first","util":0.3}],"last_switch_ts":3.0}"#;
        let state: State = serde_json::from_str(raw).unwrap();
        assert_eq!(state.accounts["a"].util_5h, Some(0.5));
        assert_eq!(state.events[0].kind, "switch");
        let back: State = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(back, state);
    }
}
