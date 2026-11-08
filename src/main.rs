//! Point d'entrée : serveur et commandes d'administration.

use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use claude_rotate::admin;
use tracing_subscriber::EnvFilter;

/// Proxy auto-hébergé qui répartit Claude Code sur plusieurs abonnements Claude.
#[derive(Debug, Parser)]
#[command(name = "claude-rotate", version, about)]
struct Cli {
    /// Répertoire contenant config.json, tokens/, logs/ et state.json.
    #[arg(
        long,
        global = true,
        value_name = "DIR",
        env = "CLAUDE_ROTATE_ROOT",
        default_value = "."
    )]
    root: PathBuf,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Lance le proxy (commande par défaut).
    Serve,
    /// Crée config.json, tokens/ et logs/ (sans rien écraser).
    Init,
    /// Enregistre le jeton `claude setup-token` d'un compte (lu masqué, ou sur stdin).
    AddAccount {
        /// Nom du compte, par exemple acct-1.
        name: String,
    },
    /// Génère une clé pour un appareil et affiche les variables à poser.
    AddDevice {
        /// Nom de l'appareil, par exemple mon-portable.
        name: String,
    },
    /// Révoque la clé d'un appareil.
    RemoveDevice {
        /// Nom de l'appareil à révoquer.
        name: String,
    },
    /// Parcours d'équipe : clé <user>-<device>, redémarrage du service, message de bienvenue.
    Onboard {
        /// Nom court de la personne, par exemple maria.
        user: String,
        /// Nom de sa machine, par exemple laptop.
        device: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("erreur : {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Journaux sur stderr : stdout reste réservé aux sorties des commandes
/// (clés, blocs à copier), exploitables en script.
fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("claude_rotate=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

fn run(cli: Cli) -> Result<()> {
    let root = cli.root.as_path();
    match cli.command.unwrap_or(Cmd::Serve) {
        Cmd::Serve => serve(root),
        Cmd::Init => admin::init(root),
        Cmd::AddAccount { name } => {
            // Le nom est vérifié avant de demander le jeton, pour ne pas faire
            // coller un secret pour rien.
            admin::validate_name("compte", &name)?;
            let token = read_token()?;
            admin::add_account(root, &name, &token)?;
            println!("compte '{name}' enregistré. Redémarrez le serveur pour le charger.");
            Ok(())
        }
        Cmd::AddDevice { name } => {
            let key = admin::add_device(root, &name)?;
            let url = admin::base_url(root)?;
            println!("appareil '{name}' enregistré. Sur cette machine :");
            println!();
            print!("{}", admin::client_env(&url, &key));
            println!();
            println!("Redémarrez le serveur pour activer la clé.");
            Ok(())
        }
        Cmd::RemoveDevice { name } => {
            admin::remove_device(root, &name)?;
            println!("appareil '{name}' révoqué. Redémarrez le serveur pour l'appliquer.");
            Ok(())
        }
        Cmd::Onboard { user, device } => admin::onboard(root, &user, &device),
    }
}

/// Le runtime tokio n'est démarré que pour le serveur : les commandes
/// d'administration restent synchrones.
fn serve(root: &Path) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("démarrage du runtime tokio")?
        .block_on(claude_rotate::server::run(root))
}

/// Saisie masquée sur un terminal ; sinon une ligne sur stdin, pour
/// `echo "$TOKEN" | claude-rotate add-account acct-1` en script.
fn read_token() -> Result<String> {
    if std::io::stdin().is_terminal() {
        eprintln!("Lancez 'claude setup-token' dans une session navigateur connectée à ce compte,");
        rpassword::prompt_password(format!(
            "puis collez le jeton {}... ici (saisie masquée) : ",
            admin::TOKEN_PREFIX
        ))
        .context("lecture du jeton")
    } else {
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .context("lecture du jeton sur stdin")?;
        Ok(line)
    }
}
