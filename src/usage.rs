//! Comptage des tokens et équivalent en dollars au tarif API.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Ligne SSE la plus longue que l'on accepte de mettre en tampon.
const MAX_SSE_LINE: usize = 4 * 1024 * 1024;
/// Prix appliqué quand aucun motif ne correspond au modèle.
pub const DEFAULT_PRICE: [f64; 2] = [5.0, 25.0];

/// Consommation d'une requête, au format d'Anthropic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    /// Lit un objet `usage` d'Anthropic ; les champs absents valent 0.
    pub fn from_json(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        let field = |k: &str| obj.get(k).and_then(Value::as_u64).unwrap_or(0);
        Some(Self {
            input_tokens: field("input_tokens"),
            output_tokens: field("output_tokens"),
            cache_read_input_tokens: field("cache_read_input_tokens"),
            cache_creation_input_tokens: field("cache_creation_input_tokens"),
        })
    }

    /// Les événements SSE répètent des compteurs cumulés : on garde le maximum.
    pub fn absorb_max(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.max(other.input_tokens);
        self.output_tokens = self.output_tokens.max(other.output_tokens);
        self.cache_read_input_tokens = self.cache_read_input_tokens.max(other.cache_read_input_tokens);
        self.cache_creation_input_tokens =
            self.cache_creation_input_tokens.max(other.cache_creation_input_tokens);
    }

    /// Tokens qui pèsent sur la fenêtre de quota (les lectures de cache exclues).
    pub fn billable_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_creation_input_tokens
    }
}

/// Découpe un flux d'octets en lignes complètes, quelle que soit la façon dont
/// le réseau a fragmenté les paquets (y compris au milieu d'un caractère UTF-8).
#[derive(Debug, Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
}

impl LineSplitter {
    /// Appelle `on_line` pour chaque ligne terminée (sans `\n` ni `\r`).
    pub fn feed(&mut self, chunk: &[u8], mut on_line: impl FnMut(&[u8])) {
        self.buf.extend_from_slice(chunk);
        let mut start = 0;
        while let Some(pos) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + pos;
            let line = &self.buf[start..end];
            on_line(line.strip_suffix(b"\r").unwrap_or(line));
            start = end + 1;
        }
        self.buf.drain(..start);
        if self.buf.len() > MAX_SSE_LINE {
            // Une ligne SSE démesurée n'est pas de la télémétrie : on l'abandonne
            // plutôt que de laisser la mémoire grossir sans limite.
            self.buf.clear();
        }
    }
}

/// Renvoie l'objet JSON d'une ligne SSE `data: {...}`.
pub fn sse_data(line: &[u8]) -> Option<Value> {
    let payload = line.strip_prefix(b"data:")?;
    serde_json::from_slice(payload.trim_ascii()).ok()
}

/// Extrait la consommation d'un flux SSE d'Anthropic sans le modifier.
#[derive(Debug, Default)]
pub struct SseUsageScanner {
    lines: LineSplitter,
    usage: Option<Usage>,
}

impl SseUsageScanner {
    pub fn feed(&mut self, chunk: &[u8]) {
        let usage = &mut self.usage;
        self.lines.feed(chunk, |line| {
            // Filtre bon marché avant tout parsing JSON.
            if !line.starts_with(b"data:") || !contains(line, b"\"usage\"") {
                return;
            }
            let Some(event) = sse_data(line) else { return };
            let found = event
                .get("usage")
                .or_else(|| event.get("message").and_then(|m| m.get("usage")))
                .and_then(Usage::from_json);
            if let Some(found) = found {
                usage.get_or_insert_with(Usage::default).absorb_max(&found);
            }
        });
    }

    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Prix `[entrée, sortie]` par million de tokens : premier motif contenu dans l'id du modèle.
pub fn price_for(prices: &IndexMap<String, [f64; 2]>, model: Option<&str>) -> [f64; 2] {
    model
        .and_then(|m| prices.iter().find(|(pattern, _)| m.contains(pattern.as_str())))
        .map_or(DEFAULT_PRICE, |(_, price)| *price)
}

/// Coût équivalent API d'une requête : entrée + sortie + lecture de cache
/// (0,1×) + écriture de cache (1,25×).
pub fn api_cost(prices: &IndexMap<String, [f64; 2]>, model: Option<&str>, usage: &Usage) -> f64 {
    let [p_in, p_out] = price_for(prices, model);
    (usage.input_tokens as f64 * p_in
        + usage.output_tokens as f64 * p_out
        + usage.cache_read_input_tokens as f64 * p_in * 0.1
        + usage.cache_creation_input_tokens as f64 * p_in * 1.25)
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_prices;

    const STREAM: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,",
        "\"cache_read_input_tokens\":300,\"output_tokens\":1}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"é\"}}\n\n",
        "event: message_delta\r\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\r\n\r\n",
    );

    #[test]
    fn scanner_reads_usage_across_events() {
        let mut s = SseUsageScanner::default();
        s.feed(STREAM.as_bytes());
        let u = s.usage().expect("usage trouvé");
        assert_eq!((u.input_tokens, u.output_tokens, u.cache_read_input_tokens), (12, 42, 300));
    }

    #[test]
    fn scanner_is_independent_of_chunking() {
        let mut s = SseUsageScanner::default();
        for byte in STREAM.as_bytes() {
            s.feed(std::slice::from_ref(byte));
        }
        assert_eq!(s.usage().map(|u| u.output_tokens), Some(42));
    }

    #[test]
    fn scanner_without_usage_reports_none() {
        let mut s = SseUsageScanner::default();
        s.feed(b"data: {\"type\":\"ping\"}\n\n");
        assert_eq!(s.usage(), None);
    }

    #[test]
    fn splitter_keeps_partial_line_for_later() {
        let mut lines = Vec::new();
        let mut sp = LineSplitter::default();
        sp.feed(b"ab\ncd", |l| lines.push(l.to_vec()));
        sp.feed(b"e\n", |l| lines.push(l.to_vec()));
        assert_eq!(lines, vec![b"ab".to_vec(), b"cde".to_vec()]);
    }

    #[test]
    fn pricing_matches_by_substring_with_default() {
        let prices = default_prices();
        assert_eq!(price_for(&prices, Some("claude-sonnet-5")), [3.0, 15.0]);
        assert_eq!(price_for(&prices, Some("inconnu")), DEFAULT_PRICE);
        assert_eq!(price_for(&prices, None), DEFAULT_PRICE);
    }

    #[test]
    fn cost_includes_cache_multipliers() {
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_input_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
        };
        let cost = api_cost(&default_prices(), Some("claude-haiku-5"), &usage);
        assert!((cost - (1.0 + 5.0 + 0.1 + 1.25)).abs() < 1e-9, "{cost}");
    }
}
