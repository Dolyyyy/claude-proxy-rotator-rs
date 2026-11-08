//! Horloge et horodatages.

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};

/// Format des horodatages de l'audit et des événements (`2026-11-08T23:24:00+0100`).
pub const TS_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%z";

/// Secondes Unix, en flottant, comme les en-têtes de réinitialisation d'Anthropic.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Horodatage local courant au format [`TS_FORMAT`].
pub fn local_timestamp() -> String {
    Local::now().format(TS_FORMAT).to_string()
}

/// Analyse un horodatage [`TS_FORMAT`] en secondes Unix.
pub fn parse_timestamp(ts: &str) -> Option<f64> {
    DateTime::parse_from_str(ts, TS_FORMAT)
        .ok()
        .map(|d| d.timestamp() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip() {
        let ts = local_timestamp();
        let parsed = parse_timestamp(&ts).expect("horodatage lisible");
        assert!((parsed - now()).abs() < 5.0);
        assert_eq!(
            parse_timestamp("2026-01-01T00:00:00+0000"),
            Some(1_767_225_600.0)
        );
        assert_eq!(parse_timestamp("n'importe quoi"), None);
    }
}
