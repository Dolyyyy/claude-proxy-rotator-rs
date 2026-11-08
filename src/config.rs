//! `config.json` : schéma, valeurs par défaut et validation au démarrage.
//!
//! Le format est volontairement identique à celui de l'implémentation Python
//! d'origine, pour qu'une installation existante puisse basculer sans rien
//! réécrire.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Fichier de configuration, relatif au répertoire racine.
pub const CONFIG_FILE: &str = "config.json";
/// État persistant (compte actif, télémétrie de quota, événements).
pub const STATE_FILE: &str = "state.json";
/// Journal d'audit, une ligne JSON par requête.
pub const AUDIT_FILE: &str = "logs/audit.jsonl";

/// Marqueur de la clé d'exemple publique de `config.example.json`.
pub const PLACEHOLDER_MARKER: &str = "REPLACE-WITH";

/// Stratégie d'ordonnancement des comptes à l'intérieur d'un même palier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Consommer d'abord le compte dont la fenêtre hebdomadaire expire le plus tôt.
    #[default]
    ConsumeFirst,
    /// Le compte le moins utilisé sur la fenêtre de 5 h.
    LeastUsed,
}

/// Un compte d'abonnement tel que décrit dans la configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct AccountConfig {
    pub name: String,
    pub token_file: PathBuf,
    /// Plus bas = préféré. Les comptes sans priorité partagent le palier par défaut.
    #[serde(default = "default_priority")]
    pub priority: i64,
    /// Compte écarté de la rotation mais toujours affiché dans le panel.
    #[serde(default)]
    pub disabled: bool,
}

fn default_priority() -> i64 {
    100
}

/// Seuils des alertes affichées dans le panel.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AlertConfig {
    pub device_tokens_per_hour: u64,
    pub expensive_model_patterns: Vec<String>,
    pub util_warn: f64,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            device_tokens_per_hour: 5_000_000,
            expensive_model_patterns: vec!["opus".into(), "fable".into(), "mythos".into()],
            util_warn: 0.8,
        }
    }
}

/// Contenu de `config.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bind: String,
    pub port: u16,
    /// URL de l'API amont (surchargée par les tests).
    pub upstream: String,
    pub switch_threshold: f64,
    pub switch_threshold_7d: f64,
    pub strategy: Strategy,
    pub switch_cooldown_s: f64,
    pub switch_margin: f64,
    pub consume_first_margin_s: f64,
    /// 0 = renvoyer le 429 quand tous les comptes sont épuisés.
    pub hold_max_s: f64,
    pub accounts: Vec<AccountConfig>,
    /// nom d'appareil -> clé d'appareil.
    pub devices: IndexMap<String, String>,
    /// Motif (sous-chaîne de l'id de modèle) -> [entrée, sortie] en $ par million de tokens.
    pub prices_per_mtok: IndexMap<String, [f64; 2]>,
    pub alerts: AlertConfig,
    pub public_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".into(),
            port: 8484,
            upstream: "https://api.anthropic.com".into(),
            switch_threshold: 0.8,
            switch_threshold_7d: 0.98,
            strategy: Strategy::ConsumeFirst,
            switch_cooldown_s: 300.0,
            switch_margin: 0.05,
            consume_first_margin_s: 3600.0,
            hold_max_s: 0.0,
            accounts: Vec::new(),
            devices: IndexMap::new(),
            prices_per_mtok: default_prices(),
            alerts: AlertConfig::default(),
            public_url: String::new(),
        }
    }
}

/// Tarifs API publics, utilisés pour la colonne « équivalent $ » du panel.
pub fn default_prices() -> IndexMap<String, [f64; 2]> {
    [
        ("fable", [10.0, 50.0]),
        ("mythos", [10.0, 50.0]),
        ("opus", [5.0, 25.0]),
        ("sonnet", [3.0, 15.0]),
        ("haiku", [1.0, 5.0]),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}

/// Un compte prêt à servir : configuration + jeton chargé depuis le disque.
#[derive(Clone)]
pub struct Account {
    pub name: String,
    pub token: String,
    pub priority: i64,
    pub disabled: bool,
}

impl std::fmt::Debug for Account {
    // Le jeton est un secret valable un an : il ne doit jamais finir dans un log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("name", &self.name)
            .field("priority", &self.priority)
            .field("disabled", &self.disabled)
            .finish_non_exhaustive()
    }
}

/// Configuration validée, avec les jetons des comptes chargés.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub accounts: Vec<Account>,
}

impl Config {
    /// Lit et désérialise `config.json`.
    pub fn read(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("lecture de {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("{} invalide", path.display()))
    }

    /// Lit la configuration sous `root`, charge les jetons et valide l'ensemble.
    pub fn load(root: &Path) -> Result<Loaded> {
        let config = Self::read(&root.join(CONFIG_FILE))?;
        config.validate()?;

        let mut accounts = Vec::with_capacity(config.accounts.len());
        for acct in &config.accounts {
            let path = root.join(&acct.token_file);
            match std::fs::read_to_string(&path) {
                Ok(raw) => {
                    let token = raw.trim().to_owned();
                    if token.is_empty() {
                        tracing::warn!(account = %acct.name, "jeton vide, compte ignoré");
                        continue;
                    }
                    accounts.push(Account {
                        name: acct.name.clone(),
                        token,
                        priority: acct.priority,
                        disabled: acct.disabled,
                    });
                }
                Err(err) => tracing::warn!(
                    account = %acct.name,
                    file = %acct.token_file.display(),
                    %err,
                    "jeton introuvable, compte ignoré"
                ),
            }
        }
        if accounts.is_empty() {
            bail!("aucun compte avec un jeton — lancez : claude-rotate add-account <nom>");
        }
        Ok(Loaded { config, accounts })
    }

    /// Refuse les configurations dangereuses ou incohérentes.
    pub fn validate(&self) -> Result<()> {
        // La clé d'exemple est publique (elle est dans le dépôt) : elle ne doit
        // jamais devenir un identifiant valide.
        let placeholders: Vec<&str> = self
            .devices
            .iter()
            .filter(|(_, key)| key.contains(PLACEHOLDER_MARKER))
            .map(|(name, _)| name.as_str())
            .collect();
        if !placeholders.is_empty() {
            bail!(
                "clé(s) d'exemple dans config.json : {} — cette valeur est publique. \
                 Lancez : claude-rotate add-device <nom> (elle remplace l'exemple).",
                placeholders.join(", ")
            );
        }

        let mut seen = HashSet::new();
        for (name, key) in &self.devices {
            if key.trim().is_empty() {
                bail!("l'appareil '{name}' a une clé vide");
            }
            if !seen.insert(key.as_str()) {
                bail!("la clé de l'appareil '{name}' est partagée avec un autre appareil");
            }
        }

        let mut names = HashSet::new();
        for acct in &self.accounts {
            if !names.insert(acct.name.as_str()) {
                bail!("compte '{}' déclaré deux fois", acct.name);
            }
        }

        for (label, value) in [
            ("switch_threshold", self.switch_threshold),
            ("switch_threshold_7d", self.switch_threshold_7d),
        ] {
            if !(0.0..=1.0).contains(&value) || value == 0.0 {
                bail!("{label} doit être dans ]0, 1] (reçu {value})");
            }
        }
        for (label, value) in [
            ("switch_cooldown_s", self.switch_cooldown_s),
            ("switch_margin", self.switch_margin),
            ("consume_first_margin_s", self.consume_first_margin_s),
            ("hold_max_s", self.hold_max_s),
        ] {
            if !value.is_finite() || value < 0.0 {
                bail!("{label} doit être un nombre positif (reçu {value})");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Config {
        serde_json::from_str(json).expect("config de test valide")
    }

    #[test]
    fn defaults_apply_to_missing_fields() {
        let cfg = parse(r#"{"accounts":[{"name":"a","token_file":"tokens/a.token"}]}"#);
        assert_eq!(cfg.port, 8484);
        assert_eq!(cfg.strategy, Strategy::ConsumeFirst);
        assert_eq!(cfg.accounts[0].priority, 100);
        assert!(!cfg.accounts[0].disabled);
        assert_eq!(cfg.alerts.util_warn, 0.8);
    }

    #[test]
    fn partial_alerts_keep_other_defaults() {
        let cfg = parse(r#"{"alerts":{"util_warn":0.5}}"#);
        assert_eq!(cfg.alerts.util_warn, 0.5);
        assert_eq!(cfg.alerts.device_tokens_per_hour, 5_000_000);
    }

    #[test]
    fn strategy_is_kebab_case() {
        assert_eq!(parse(r#"{"strategy":"least-used"}"#).strategy, Strategy::LeastUsed);
    }

    #[test]
    fn placeholder_device_key_is_refused() {
        let cfg = parse(r#"{"devices":{"laptop":"dev-laptop-REPLACE-WITH-RANDOM-HEX"}}"#);
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("laptop"), "{err}");
    }

    #[test]
    fn duplicate_device_keys_are_refused() {
        let cfg = parse(r#"{"devices":{"a":"k1","b":"k1"}}"#);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn out_of_range_threshold_is_refused() {
        assert!(parse(r#"{"switch_threshold":1.5}"#).validate().is_err());
        assert!(parse(r#"{"hold_max_s":-1}"#).validate().is_err());
    }

    #[test]
    fn example_config_parses_once_placeholder_replaced() {
        let raw = include_str!("../config.example.json").replace(PLACEHOLDER_MARKER, "x");
        let cfg: Config = serde_json::from_str(&raw).expect("config.example.json valide");
        cfg.validate().expect("exemple valide une fois la clé remplacée");
    }

    #[test]
    fn load_skips_missing_tokens_and_requires_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("tokens")).unwrap();
        std::fs::write(dir.path().join("tokens/a.token"), "sk-ant-oat01-a\n").unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"accounts":[{"name":"a","token_file":"tokens/a.token"},
                            {"name":"b","token_file":"tokens/b.token"}],
                "devices":{"laptop":"dev-1"}}"#,
        )
        .unwrap();
        let loaded = Config::load(dir.path()).unwrap();
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(loaded.accounts[0].token, "sk-ant-oat01-a");

        std::fs::remove_file(dir.path().join("tokens/a.token")).unwrap();
        assert!(Config::load(dir.path()).is_err());
    }
}
