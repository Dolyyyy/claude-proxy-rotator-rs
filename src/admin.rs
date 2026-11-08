//! Commandes d'administration : préparation du répertoire, comptes, appareils.
//!
//! Remplace `setup.sh` de l'implémentation Python, sans dépendre de bash,
//! d'openssl ni de python3 : les mêmes opérations fonctionnent sous Windows,
//! Linux et macOS.
//!
//! `config.json` est toujours édité sous forme de [`serde_json::Value`] et non
//! via [`crate::config::Config`] : les champs inconnus et l'ordre des clés
//! sont ainsi conservés, et un fichier écrit par l'administrateur n'est jamais
//! « normalisé » dans son dos.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Map, Value, json};

use crate::config::{CONFIG_FILE, PLACEHOLDER_MARKER};

/// Configuration d'exemple embarquée dans le binaire : `init` fonctionne même
/// sans le dépôt source à côté.
const EXAMPLE_CONFIG: &str = include_str!("../config.example.json");

/// Préfixe des jetons produits par `claude setup-token`.
pub const TOKEN_PREFIX: &str = "sk-ant-oat01-";

/// Répertoire des jetons de compte, relatif à la racine.
const TOKENS_DIR: &str = "tokens";
/// Répertoire des journaux, relatif à la racine.
const LOGS_DIR: &str = "logs";

/// Nom de l'unité systemd redémarrée par `onboard`.
const SERVICE_NAME: &str = "claude-rotate";

/// Longueur maximale d'un nom de compte ou d'appareil.
const MAX_NAME_LEN: usize = 64;

/// Noms de périphériques réservés par Windows : `tokens/con.token` ne serait
/// pas un fichier ordinaire.
const WINDOWS_RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Prépare `root` : `tokens/`, `logs/` et `config.json` (copié de l'exemple
/// s'il n'existe pas). Idempotent : un `config.json` existant n'est jamais
/// écrasé.
pub fn init(root: &Path) -> Result<()> {
    create_private_dir(&root.join(TOKENS_DIR))?;
    let logs = root.join(LOGS_DIR);
    fs::create_dir_all(&logs).with_context(|| format!("création de {}", logs.display()))?;

    let config = root.join(CONFIG_FILE);
    if config.exists() {
        println!("{} existe déjà, laissé tel quel", config.display());
    } else {
        write_private(&config, EXAMPLE_CONFIG.as_bytes())?;
        println!("{} créé depuis config.example.json", config.display());
    }

    let cmd = command_prefix(root);
    println!();
    println!("Prêt. Étapes suivantes :");
    println!("  {cmd} add-account acct-1      # jeton issu de : claude setup-token");
    println!("  {cmd} add-device mon-portable # affiche les variables à poser sur l'appareil");
    println!("  {cmd} serve");
    Ok(())
}

/// Enregistre un appareil et renvoie sa clé (`dev-<nom>-<16 hex>`).
///
/// Les clés d'exemple (publiques, puisque présentes dans le dépôt) sont
/// supprimées au passage ; un nom déjà enregistré est refusé plutôt que
/// remplacé, pour ne pas révoquer une clé en service par erreur.
pub fn add_device(root: &Path, name: &str) -> Result<String> {
    validate_name("appareil", name)?;
    let mut config = read_config(root)?;
    let devices = object_field(&mut config, "devices")?;

    devices.retain(|_, key| !key.as_str().is_some_and(|k| k.contains(PLACEHOLDER_MARKER)));
    if devices.contains_key(name) {
        bail!(
            "l'appareil '{name}' existe déjà — révoquez-le d'abord \
             (claude-rotate remove-device {name}) ou choisissez un autre nom"
        );
    }

    let key = format!("dev-{name}-{}", random_hex(8)?);
    devices.insert(name.to_owned(), Value::String(key.clone()));
    write_config(root, &config)?;
    Ok(key)
}

/// Révoque un appareil en retirant sa clé de la configuration.
pub fn remove_device(root: &Path, name: &str) -> Result<()> {
    let mut config = read_config(root)?;
    let devices = object_field(&mut config, "devices")?;
    // shift_remove et non remove : ce dernier permute les entrées et casserait
    // l'ordre du fichier.
    if devices.shift_remove(name).is_none() {
        bail!("appareil inconnu : '{name}'");
    }
    write_config(root, &config)
}

/// Stocke le jeton d'un compte dans `tokens/<nom>.token` et déclare le compte
/// dans la configuration s'il n'y figure pas encore.
pub fn add_account(root: &Path, name: &str, token: &str) -> Result<()> {
    validate_name("compte", name)?;
    let token = token.trim();
    ensure!(
        token.len() > TOKEN_PREFIX.len() && token.starts_with(TOKEN_PREFIX),
        "le jeton doit commencer par {TOKEN_PREFIX} (obtenu avec : claude setup-token)"
    );

    let mut config = read_config(root)?;
    let accounts = match config
        .entry("accounts")
        .or_insert_with(|| Value::Array(Vec::new()))
    {
        Value::Array(list) => list,
        _ => bail!("'accounts' doit être une liste dans {CONFIG_FILE}"),
    };

    // Un compte déjà déclaré garde son token_file : réécrire le jeton ailleurs
    // laisserait le serveur lire l'ancien.
    let existing = accounts
        .iter()
        .find(|acct| acct.get("name").and_then(Value::as_str) == Some(name))
        .map(|acct| {
            acct.get("token_file")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let is_new = existing.is_none();
    let token_file = match existing {
        Some(Some(file)) => file,
        Some(None) => bail!("le compte '{name}' n'a pas de token_file dans {CONFIG_FILE}"),
        None => {
            let file = format!("{TOKENS_DIR}/{name}.token");
            accounts.push(json!({ "name": name, "token_file": file }));
            file
        }
    };

    let path = root.join(&token_file);
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    write_private(&path, token.as_bytes())?;
    if is_new {
        write_config(root, &config)?;
    }
    Ok(())
}

/// Parcours d'équipe : enregistre l'appareil `<user>-<device>`, tente de
/// redémarrer le service pour activer la clé, puis affiche un message de
/// bienvenue prêt à transmettre à l'intéressé.
pub fn onboard(root: &Path, user: &str, device: &str) -> Result<()> {
    validate_name("utilisateur", user)?;
    validate_name("appareil", device)?;
    let name = format!("{user}-{device}");
    let key = add_device(root, &name)?;
    let url = base_url(root)?;

    println!("{}", restart_service().message());
    println!();
    print!("{}", welcome_message(user, device, &url, &key));
    Ok(())
}

/// URL à donner aux appareils : `public_url` de la configuration, sinon un
/// gabarit à compléter avec l'adresse du serveur.
pub fn base_url(root: &Path) -> Result<String> {
    let config = read_config(root)?;
    let public = config
        .get("public_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .trim_end_matches('/');
    if !public.is_empty() {
        return Ok(public.to_owned());
    }
    let port = config.get("port").and_then(Value::as_u64).unwrap_or(8484);
    Ok(format!("http://<this-host>:{port}"))
}

/// Variables d'environnement à poser sur un appareil, en syntaxe bash/zsh et
/// PowerShell.
pub fn client_env(url: &str, key: &str) -> String {
    format!(
        "  bash / zsh (~/.bashrc, ~/.zshrc) :\n\
         \x20   export ANTHROPIC_BASE_URL={url}\n\
         \x20   export CLAUDE_CODE_OAUTH_TOKEN={key}\n\
         \n\
         \x20 PowerShell ($PROFILE) :\n\
         \x20   $env:ANTHROPIC_BASE_URL = \"{url}\"\n\
         \x20   $env:CLAUDE_CODE_OAUTH_TOKEN = \"{key}\"\n"
    )
}

/// Bloc de bienvenue d'`onboard`, à coller dans un canal raisonnablement
/// privé : la clé est un secret au porteur.
pub fn welcome_message(user: &str, device: &str, url: &str, key: &str) -> String {
    format!(
        "──── à transmettre à {user} ────────────────────────────────\n\
         Bonjour {user}, ton accès Claude pour '{device}' est prêt.\n\
         \n\
         Ajoute ces lignes au profil de ton shell :\n\
         \n\
         {env}\
         \n\
         Ouvre un nouveau terminal et lance 'claude' comme d'habitude, sans connexion.\n\
         Tableau de bord : {url}/rotate/panel?key={key}\n\
         La clé est propre à cet appareil : ne la partage pas et ne la committe pas.\n\
         ──────────────────────────────────────────────────────────────\n",
        env = client_env(url, key),
    )
}

/// Vérifie qu'un nom est utilisable comme nom de fichier et comme clé de
/// configuration : `[A-Za-z0-9._-]`, sans point initial (ni `.`, ni `..`, ni
/// fichier caché), et pas un nom réservé de Windows.
pub fn validate_name(kind: &str, name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "nom d'{kind} vide");
    ensure!(
        name.len() <= MAX_NAME_LEN,
        "nom d'{kind} trop long ({} caractères, maximum {MAX_NAME_LEN})",
        name.len()
    );
    ensure!(
        name.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
        "nom d'{kind} invalide : '{name}' (caractères autorisés : A-Z a-z 0-9 . _ -)"
    );
    ensure!(
        !name.starts_with('.'),
        "nom d'{kind} invalide : '{name}' ne doit pas commencer par un point"
    );
    let stem = name.split('.').next().unwrap_or_default();
    ensure!(
        !WINDOWS_RESERVED.contains(&stem.to_ascii_lowercase().as_str()),
        "nom d'{kind} invalide : '{name}' est réservé sous Windows"
    );
    Ok(())
}

/// Issue de la tentative de redémarrage du service après `onboard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartOutcome {
    Restarted,
    /// Service actif mais redémarrage refusé (droits insuffisants).
    NotLive,
    /// Pas de service systemd actif : le serveur est lancé autrement.
    Unmanaged,
}

impl RestartOutcome {
    fn message(self) -> &'static str {
        match self {
            Self::Restarted => "(service redémarré : la clé est active)",
            Self::NotLive => {
                "(clé enregistrée mais PAS encore active : sudo systemctl restart claude-rotate)"
            }
            Self::Unmanaged => "(redémarrez claude-rotate serve pour activer la clé)",
        }
    }
}

/// Redémarre l'unité systemd si elle tourne. `sudo -n` échoue au lieu de
/// demander un mot de passe : le bloc de bienvenue doit s'afficher quoi qu'il
/// arrive, la clé étant déjà enregistrée.
fn restart_service() -> RestartOutcome {
    if !cfg!(target_os = "linux") {
        return RestartOutcome::Unmanaged;
    }
    if !run_quietly("systemctl", &["is-active", "--quiet", SERVICE_NAME]) {
        return RestartOutcome::Unmanaged;
    }
    if run_quietly("systemctl", &["restart", SERVICE_NAME])
        || run_quietly("sudo", &["-n", "systemctl", "restart", SERVICE_NAME])
    {
        RestartOutcome::Restarted
    } else {
        RestartOutcome::NotLive
    }
}

/// Lance une commande sans entrée ni sortie ; `false` si elle est absente ou
/// échoue.
fn run_quietly(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Préfixe à afficher dans les commandes suggérées.
fn command_prefix(root: &Path) -> String {
    if root == Path::new(".") {
        "claude-rotate".to_owned()
    } else {
        format!("claude-rotate --root {}", root.display())
    }
}

/// `n` octets aléatoires du système, en hexadécimal.
fn random_hex(n: usize) -> Result<String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes)
        .map_err(|err| anyhow!("générateur aléatoire indisponible : {err}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn config_path(root: &Path) -> PathBuf {
    root.join(CONFIG_FILE)
}

/// Lit `config.json` comme objet JSON brut.
fn read_config(root: &Path) -> Result<Map<String, Value>> {
    let path = config_path(root);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => bail!(
            "{} introuvable — lancez d'abord : {} init",
            path.display(),
            command_prefix(root)
        ),
        Err(err) => return Err(err).with_context(|| format!("lecture de {}", path.display())),
    };
    match serde_json::from_str(&raw).with_context(|| format!("{} invalide", path.display()))? {
        Value::Object(map) => Ok(map),
        _ => bail!("{} doit contenir un objet JSON", path.display()),
    }
}

/// Réécrit `config.json` atomiquement, indenté sur 2 espaces comme l'original.
fn write_config(root: &Path, config: &Map<String, Value>) -> Result<()> {
    let mut raw = serde_json::to_string_pretty(config).context("sérialisation de la config")?;
    raw.push('\n');
    write_private(&config_path(root), raw.as_bytes())
}

/// Champ objet de la configuration, créé vide s'il est absent.
fn object_field<'a>(
    config: &'a mut Map<String, Value>,
    field: &str,
) -> Result<&'a mut Map<String, Value>> {
    match config
        .entry(field)
        .or_insert_with(|| Value::Object(Map::new()))
    {
        Value::Object(map) => Ok(map),
        _ => bail!("'{field}' doit être un objet dans {CONFIG_FILE}"),
    }
}

/// Crée un répertoire réservé au propriétaire (0700 sous unix).
fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("création de {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("droits de {}", path.display()))?;
    }
    Ok(())
}

/// Écrit un fichier sensible de façon atomique (fichier temporaire voisin puis
/// renommage) : un arrêt brutal ne laisse jamais une config tronquée. Droits
/// 0600 sous unix, posés dès la création.
fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .with_context(|| format!("chemin sans nom de fichier : {}", path.display()))?;
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));

    let result = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            options.mode(0o600);
            let file = options.open(&tmp)?;
            // Un temporaire laissé par un essai précédent garde ses anciens droits.
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            write_and_sync(file, contents)?;
        }
        #[cfg(not(unix))]
        write_and_sync(options.open(&tmp)?, contents)?;
        fs::rename(&tmp, path)
    })();

    if result.is_err() {
        // Best effort : le temporaire n'a plus d'utilité.
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("écriture de {}", path.display()))
}

fn write_and_sync(mut file: fs::File, contents: &[u8]) -> std::io::Result<()> {
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn initialized() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        dir
    }

    fn raw_config(dir: &Path) -> String {
        fs::read_to_string(dir.join(CONFIG_FILE)).unwrap()
    }

    #[test]
    fn init_is_idempotent() {
        let dir = initialized();
        assert!(dir.path().join(TOKENS_DIR).is_dir());
        assert!(dir.path().join(LOGS_DIR).is_dir());
        assert_eq!(raw_config(dir.path()), EXAMPLE_CONFIG);

        fs::write(dir.path().join(CONFIG_FILE), "{\"port\": 9000}").unwrap();
        init(dir.path()).unwrap();
        assert_eq!(raw_config(dir.path()), "{\"port\": 9000}");
    }

    #[cfg(unix)]
    #[test]
    fn init_restricts_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = initialized();
        let mode = |p: &str| {
            fs::metadata(dir.path().join(p))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(TOKENS_DIR), 0o700);
        assert_eq!(mode(CONFIG_FILE), 0o600);
    }

    #[test]
    fn add_device_replaces_placeholder_and_validates() {
        let dir = initialized();
        let key = add_device(dir.path(), "laptop").unwrap();
        assert!(key.starts_with("dev-laptop-"), "{key}");
        let suffix = key.trim_start_matches("dev-laptop-");
        assert_eq!(suffix.len(), 16);
        assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()));

        let cfg = Config::read(&dir.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(cfg.devices.len(), 1);
        assert_eq!(cfg.devices["laptop"], key);
        cfg.validate().unwrap();
    }

    #[test]
    fn add_device_refuses_duplicates() {
        let dir = initialized();
        let key = add_device(dir.path(), "laptop").unwrap();
        assert!(add_device(dir.path(), "laptop").is_err());
        let cfg = Config::read(&dir.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(cfg.devices["laptop"], key, "la clé existante est intacte");
        assert_ne!(add_device(dir.path(), "desktop").unwrap(), key);
    }

    #[test]
    fn add_device_requires_init() {
        let dir = tempfile::tempdir().unwrap();
        let err = add_device(dir.path(), "laptop").unwrap_err().to_string();
        assert!(err.contains("init"), "{err}");
    }

    #[test]
    fn remove_device_revokes_and_keeps_order() {
        let dir = initialized();
        for name in ["a", "b", "c"] {
            add_device(dir.path(), name).unwrap();
        }
        remove_device(dir.path(), "b").unwrap();
        let cfg = Config::read(&dir.path().join(CONFIG_FILE)).unwrap();
        let names: Vec<_> = cfg.devices.keys().map(String::as_str).collect();
        assert_eq!(names, ["a", "c"]);
        assert!(remove_device(dir.path(), "b").is_err());
    }

    #[test]
    fn add_account_stores_token_and_registers_once() {
        let dir = initialized();
        add_account(dir.path(), "acct-3", "  sk-ant-oat01-secret\n").unwrap();
        let token = fs::read_to_string(dir.path().join("tokens/acct-3.token")).unwrap();
        assert_eq!(token, "sk-ant-oat01-secret");

        add_account(dir.path(), "acct-3", "sk-ant-oat01-renewed").unwrap();
        let token = fs::read_to_string(dir.path().join("tokens/acct-3.token")).unwrap();
        assert_eq!(token, "sk-ant-oat01-renewed");

        let cfg = Config::read(&dir.path().join(CONFIG_FILE)).unwrap();
        let names: Vec<_> = cfg.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["acct-1", "acct-2", "acct-3"]);
    }

    #[test]
    fn add_account_refuses_bad_token() {
        let dir = initialized();
        assert!(add_account(dir.path(), "acct-3", "sk-ant-api03-nope").is_err());
        assert!(add_account(dir.path(), "acct-3", TOKEN_PREFIX).is_err());
        assert!(!dir.path().join("tokens/acct-3.token").exists());
    }

    #[test]
    fn add_account_refuses_invalid_names() {
        let dir = initialized();
        for name in [
            "", "..", "../evil", "a/b", "a\\b", ".hidden", "con", "nul.txt", "é",
        ] {
            assert!(
                add_account(dir.path(), name, "sk-ant-oat01-x").is_err(),
                "nom accepté à tort : {name:?}"
            );
        }
        assert!(add_account(dir.path(), &"a".repeat(MAX_NAME_LEN + 1), "sk-ant-oat01-x").is_err());
        let entries: Vec<_> = fs::read_dir(dir.path().join(TOKENS_DIR)).unwrap().collect();
        assert!(entries.is_empty());
        assert!(!dir.path().join("evil.token").exists());
    }

    #[test]
    fn edits_preserve_unknown_fields_and_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"zeta": 1, "devices": {}, "custom": {"nested": [1, 2]}, "accounts": [], "alpha": true}"#,
        )
        .unwrap();
        add_device(dir.path(), "laptop").unwrap();
        add_account(dir.path(), "main", "sk-ant-oat01-x").unwrap();

        let value: Value = serde_json::from_str(&raw_config(dir.path())).unwrap();
        let keys: Vec<_> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["zeta", "devices", "custom", "accounts", "alpha"]);
        assert_eq!(value["custom"], json!({"nested": [1, 2]}));
        assert_eq!(value["accounts"][0]["token_file"], "tokens/main.token");
        assert!(
            raw_config(dir.path()).contains("\n  \"zeta\": 1,"),
            "indentation 2 espaces"
        );
    }

    #[test]
    fn onboard_refuses_existing_device() {
        let dir = initialized();
        add_device(dir.path(), "maria-laptop").unwrap();
        assert!(onboard(dir.path(), "maria", "laptop").is_err());
        assert!(onboard(dir.path(), "maria", "../x").is_err());
    }

    #[test]
    fn base_url_prefers_public_url() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(CONFIG_FILE), r#"{"port": 9000}"#).unwrap();
        assert_eq!(base_url(dir.path()).unwrap(), "http://<this-host>:9000");
        fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"public_url": "https://rotate.example.com/"}"#,
        )
        .unwrap();
        assert_eq!(base_url(dir.path()).unwrap(), "https://rotate.example.com");
    }

    #[test]
    fn client_env_covers_bash_and_powershell() {
        let env = client_env("http://h:8484", "dev-x-1");
        assert!(env.contains("export ANTHROPIC_BASE_URL=http://h:8484"));
        assert!(env.contains("export CLAUDE_CODE_OAUTH_TOKEN=dev-x-1"));
        assert!(env.contains("$env:ANTHROPIC_BASE_URL = \"http://h:8484\""));
        assert!(env.contains("$env:CLAUDE_CODE_OAUTH_TOKEN = \"dev-x-1\""));
    }

    #[test]
    fn welcome_message_is_paste_ready() {
        let msg = welcome_message("maria", "laptop", "http://h:8484", "dev-maria-laptop-1");
        assert!(msg.contains("http://h:8484/rotate/panel?key=dev-maria-laptop-1"));
        assert!(msg.contains("export CLAUDE_CODE_OAUTH_TOKEN=dev-maria-laptop-1"));
        // Aucune indentation parasite héritée du littéral multiligne.
        assert!(msg.lines().all(|line| !line.starts_with("     ")), "{msg}");
    }
}
