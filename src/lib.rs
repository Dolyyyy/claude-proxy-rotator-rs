//! claude-rotate : proxy auto-hébergé qui répartit Claude Code sur plusieurs
//! abonnements Claude et bascule de compte quand une fenêtre de quota se remplit.
//!
//! Organisation :
//! - [`config`] : `config.json`, valeurs par défaut, validation ;
//! - [`rotation`] : politique de rotation, pure et testable ;
//! - [`usage`] : comptage des tokens et équivalent en dollars ;
//! - [`audit`] : journal d'audit et agrégats du panel ;
//! - [`oai`] : traduction OpenAI chat.completions <-> Anthropic messages ;
//! - [`server`] : serveur HTTP, relais amont et boucle de rotation ;
//! - [`admin`] : commandes d'administration (comptes, appareils).

pub mod admin;
pub mod audit;
pub mod config;
pub mod oai;
pub mod rotation;
pub mod server;
pub mod time;
pub mod usage;
