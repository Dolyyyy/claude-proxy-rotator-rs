//! Traduction OpenAI `chat.completions` <-> Anthropic `/v1/messages`.
//!
//! Périmètre : conversations textuelles. L'usage d'outils est refusé
//! explicitement (400) plutôt que mal traduit : les schémas diffèrent, et une
//! traduction approximative passerait pour une défaillance du modèle.
//!
//! Les sorties sont des `Value` construites directement (`json!`, `Map`) : avec
//! `preserve_order`, l'ordre des clés suit l'insertion, et contrairement à une
//! sérialisation de structs la construction ne peut pas échouer.

use serde_json::{Map, Value, json};

use crate::usage::{LineSplitter, Usage, sse_data};

/// Identifiant renvoyé quand Anthropic n'en fournit pas.
const DEFAULT_ID: &str = "chatcmpl-rotate";
/// `max_tokens` imposé par Anthropic, appliqué quand le client n'en donne pas.
const DEFAULT_MAX_TOKENS: u64 = 4096;

/// Vérité « à la Python » : `null`, `false`, `0`, `""`, `[]` et `{}` valent absent.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Première valeur « vraie » parmi les clés données.
fn first_truthy<'a>(obj: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .map(|k| obj.get(*k))
        .find(|v| truthy(*v))
        .flatten()
}

/// Raison d'arrêt Anthropic -> `finish_reason` OpenAI.
fn finish_reason(stop_reason: Option<&str>) -> &'static str {
    match stop_reason {
        Some("max_tokens") => "length",
        _ => "stop",
    }
}

/// OpenAI compte tout le prompt ; Anthropic isole lectures et écritures de
/// cache. Les replier dans `prompt_tokens` garde justes les calculs de coût
/// côté client.
fn oai_usage(usage: &Usage) -> Value {
    let prompt = usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(usage.cache_creation_input_tokens);
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": usage.output_tokens,
        "total_tokens": prompt.saturating_add(usage.output_tokens),
    })
}

/// Concatène les blocs `{"type": "text", "text": ...}` d'un tableau.
fn join_text_parts(parts: &[Value]) -> String {
    parts
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect()
}

/// Contenu d'un message OpenAI ramené à du texte brut.
fn message_text(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => join_text_parts(parts),
        Some(other) => other.to_string(),
    }
}

/// `max_tokens` accepté en entier, en flottant (tronqué) ou en chaîne entière,
/// comme le `int()` de Python ; toute autre valeur donne une erreur 400.
fn parse_max_tokens(value: &Value) -> Result<u64, String> {
    let parsed = match value {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && *f >= 1.0 && *f < u64::MAX as f64)
                .map(|f| f.trunc() as u64)
        }),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    };
    parsed
        .filter(|n| *n >= 1)
        .ok_or_else(|| format!("max_tokens must be a positive integer, got {value}"))
}

/// Corps OpenAI -> corps Anthropic, ou message d'erreur pour un 400.
///
/// Les messages system/developer deviennent le paramètre `system` ; les
/// messages consécutifs de même rôle sont fusionnés, car Anthropic exige
/// l'alternance user/assistant.
pub fn oai_to_anthropic(oai: &Value) -> Result<Value, String> {
    let empty = Map::new();
    let obj = oai.as_object().unwrap_or(&empty);
    if ["tools", "functions", "tool_choice"]
        .iter()
        .any(|k| truthy(obj.get(*k)))
    {
        return Err("tool use is not supported by this endpoint yet".into());
    }

    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<(&'static str, String)> = Vec::new();
    let entries = obj
        .get("messages")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for entry in entries {
        let role = entry.get("role").unwrap_or(&Value::Null);
        let content = message_text(entry.get("content"));
        let role = match role.as_str() {
            Some("system" | "developer") => {
                system_parts.push(content);
                continue;
            }
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => return Err(format!("unsupported message role: {role}")),
        };
        match messages.last_mut() {
            Some((last_role, last)) if *last_role == role => {
                last.push_str("\n\n");
                last.push_str(&content);
            }
            _ => messages.push((role, content)),
        }
    }
    if messages.is_empty() {
        return Err("messages[] must contain at least one user/assistant message".into());
    }

    let max_tokens = match first_truthy(obj, &["max_tokens", "max_completion_tokens"]) {
        Some(value) => parse_max_tokens(value)?,
        None => DEFAULT_MAX_TOKENS,
    };
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect();
    let mut body = Map::new();
    body.insert(
        "model".into(),
        obj.get("model").cloned().unwrap_or(Value::Null),
    );
    body.insert("max_tokens".into(), max_tokens.into());
    body.insert("messages".into(), messages.into());
    body.insert("stream".into(), truthy(obj.get("stream")).into());
    if !system_parts.is_empty() {
        body.insert("system".into(), system_parts.join("\n\n").into());
    }
    for key in ["temperature", "top_p"] {
        if let Some(value) = obj.get(key).filter(|v| !v.is_null()) {
            body.insert(key.into(), value.clone());
        }
    }
    if let Some(stop) = obj.get("stop").filter(|v| truthy(Some(v))) {
        let sequences = match stop {
            Value::String(_) => vec![stop.clone()],
            Value::Array(items) => items.clone(),
            _ => return Err("stop must be a string or a list of strings".into()),
        };
        body.insert("stop_sequences".into(), sequences.into());
    }
    Ok(body.into())
}

/// Réponse Anthropic `message` -> réponse OpenAI `chat.completion`.
pub fn anthropic_to_oai(msg: &Value) -> Value {
    let text = msg
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| join_text_parts(blocks))
        .unwrap_or_default();
    let usage = msg
        .get("usage")
        .and_then(Usage::from_json)
        .unwrap_or_default();
    json!({
        "id": msg.get("id").filter(|v| truthy(Some(v))).cloned().unwrap_or(DEFAULT_ID.into()),
        "object": "chat.completion",
        "created": unix_now(),
        "model": msg.get("model").cloned().unwrap_or(Value::Null),
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": finish_reason(msg.get("stop_reason").and_then(Value::as_str)),
        }],
        "usage": oai_usage(&usage),
    })
}

fn unix_now() -> u64 {
    crate::time::now() as u64
}

/// Traducteur incrémental SSE Anthropic -> chunks OpenAI.
#[derive(Debug)]
pub struct SseToOai {
    lines: LineSplitter,
    state: StreamState,
}

/// État accumulé au fil des événements, séparé du tampon de lignes pour
/// pouvoir être emprunté dans le rappel du [`LineSplitter`].
#[derive(Debug)]
struct StreamState {
    id: Value,
    model: Value,
    created: u64,
    usage: Option<Usage>,
    finish_reason: Option<&'static str>,
}

impl StreamState {
    fn chunk(&self, delta: Value, finish: Option<&str>) -> String {
        let chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        });
        format!("data: {chunk}\n\n")
    }

    /// Les compteurs présents écrasent les précédents (message_delta répète le
    /// cumul de sortie) ; les absents restent inchangés.
    fn absorb_usage(&mut self, usage: Option<&Value>) {
        let Some(obj) = usage.and_then(Value::as_object) else {
            return;
        };
        let read = |k: &str| obj.get(k).and_then(Value::as_u64);
        let incoming = [
            read("input_tokens"),
            read("output_tokens"),
            read("cache_read_input_tokens"),
            read("cache_creation_input_tokens"),
        ];
        if incoming.iter().all(Option::is_none) {
            return;
        }
        let u = self.usage.get_or_insert_default();
        let slots = [
            &mut u.input_tokens,
            &mut u.output_tokens,
            &mut u.cache_read_input_tokens,
            &mut u.cache_creation_input_tokens,
        ];
        for (slot, value) in slots.into_iter().zip(incoming) {
            if let Some(value) = value {
                *slot = value;
            }
        }
    }

    fn handle(&mut self, event: &Value, out: &mut String) {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let message = event.get("message");
                let field = |k: &str| message.and_then(|m| m.get(k)).filter(|v| truthy(Some(v)));
                if let Some(id) = field("id") {
                    self.id = id.clone();
                }
                if let Some(model) = field("model") {
                    self.model = model.clone();
                }
                self.absorb_usage(message.and_then(|m| m.get("usage")));
                out.push_str(&self.chunk(json!({"role": "assistant", "content": ""}), None));
            }
            Some("content_block_delta") => {
                let delta = event.get("delta");
                let is_text =
                    delta.and_then(|d| d.get("type")).and_then(Value::as_str) == Some("text_delta");
                let text = delta.and_then(|d| d.get("text")).and_then(Value::as_str);
                if let (true, Some(text)) = (is_text, text.filter(|t| !t.is_empty())) {
                    out.push_str(&self.chunk(json!({"content": text}), None));
                }
            }
            Some("message_delta") => {
                let stop = event.get("delta").and_then(|d| d.get("stop_reason"));
                if truthy(stop) {
                    self.finish_reason = Some(finish_reason(stop.and_then(Value::as_str)));
                }
                self.absorb_usage(event.get("usage"));
            }
            _ => {}
        }
    }
}

impl SseToOai {
    pub fn new() -> Self {
        Self {
            lines: LineSplitter::default(),
            state: StreamState {
                id: DEFAULT_ID.into(),
                model: Value::Null,
                created: unix_now(),
                usage: None,
                finish_reason: None,
            },
        }
    }

    /// Consomme des octets amont bruts, découpés n'importe où, et renvoie les
    /// lignes `data:` OpenAI prêtes à émettre.
    pub fn feed(&mut self, chunk: &[u8]) -> String {
        let mut out = String::new();
        let state = &mut self.state;
        self.lines.feed(chunk, |line| {
            if let Some(event) = sse_data(line) {
                state.handle(&event, &mut out);
            }
        });
        out
    }

    /// Fin de flux : chunk portant le `finish_reason`, chunk d'usage, `[DONE]`.
    pub fn tail(&self) -> String {
        let state = &self.state;
        let usage_chunk = json!({
            "id": state.id,
            "object": "chat.completion.chunk",
            "created": state.created,
            "model": state.model,
            "choices": [],
            "usage": oai_usage(&state.usage.unwrap_or_default()),
        });
        let finish = state.chunk(json!({}), Some(state.finish_reason.unwrap_or("stop")));
        format!("{finish}data: {usage_chunk}\n\ndata: [DONE]\n\n")
    }

    /// Usage au format Anthropic cumulé sur message_start et message_delta.
    pub fn usage(&self) -> Option<Usage> {
        self.state.usage
    }
}

impl Default for SseToOai {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(body: Value) -> Value {
        oai_to_anthropic(&body).expect("traduction acceptée")
    }

    #[test]
    fn request_translation_full_shape() {
        let body = translate(json!({
            "model": "claude-sonnet-5",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "system", "content": "in greek"},
                {"role": "user", "content": [{"type": "text", "text": "γεια"},
                                             {"type": "text", "text": " σου"}]},
                {"role": "assistant", "content": "γεια!"},
                {"role": "user", "content": "a"},
                {"role": "user", "content": "b"},
            ],
            "max_tokens": 128, "temperature": 0.5, "top_p": 0.9, "stop": "END",
            "stream": true,
        }));
        assert_eq!(body["system"], "be terse\n\nin greek");
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "γεια σου"},
                {"role": "assistant", "content": "γεια!"},
                {"role": "user", "content": "a\n\nb"},
            ])
        );
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["stop_sequences"], json!(["END"]));
        assert_eq!(body["stream"], true);
        let keys: Vec<&str> = body
            .as_object()
            .expect("objet")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "model",
                "max_tokens",
                "messages",
                "stream",
                "system",
                "temperature",
                "top_p",
                "stop_sequences"
            ]
        );
    }

    #[test]
    fn request_defaults_and_max_completion_tokens() {
        let body = translate(json!({"model": "m", "max_completion_tokens": 64,
                                    "messages": [{"role": "user", "content": "hi"}]}));
        assert_eq!(body["max_tokens"], 64);
        assert_eq!(body["stream"], false);
        assert!(body.get("temperature").is_none() && body.get("system").is_none());

        let body = translate(json!({"messages": [{"role": "user", "content": "hi"}],
                                    "max_tokens": 0, "temperature": null}));
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(body["model"], Value::Null);
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn max_tokens_accepts_numeric_forms_and_rejects_garbage() {
        let with = |v: Value| {
            oai_to_anthropic(&json!({"messages": [{"role": "user", "content": "x"}],
                                     "max_tokens": v}))
        };
        assert_eq!(
            with(json!(12.9)).map(|b| b["max_tokens"].clone()),
            Ok(json!(12))
        );
        assert_eq!(
            with(json!(" 256 ")).map(|b| b["max_tokens"].clone()),
            Ok(json!(256))
        );
        for bad in [json!("abc"), json!(-5), json!([1]), json!(true), json!(0.5)] {
            assert!(
                with(bad.clone()).is_err_and(|e| e.contains("max_tokens")),
                "{bad}"
            );
        }
    }

    #[test]
    fn stop_list_and_content_edge_cases() {
        let body = translate(json!({
            "messages": [
                {"role": "user", "content": null},
                {"role": "user", "content": [{"type": "image_url"}, "brut", {"type": "text", "text": "ok"}]},
            ],
            "stop": ["A", "B"],
        }));
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "\n\nok"}])
        );
        assert_eq!(body["stop_sequences"], json!(["A", "B"]));
        let body = translate(json!({"messages": [{"role": "user", "content": "x"}], "stop": ""}));
        assert!(body.get("stop_sequences").is_none());
    }

    #[test]
    fn tools_and_unknown_roles_are_refused_not_mistranslated() {
        let user = json!([{"role": "user", "content": "x"}]);
        let err = oai_to_anthropic(&json!({"messages": user, "tools": [{"type": "function"}]}))
            .expect_err("outils refusés");
        assert!(err.contains("tool"));
        let err = oai_to_anthropic(&json!({"messages": [{"role": "tool", "content": "x"}]}))
            .expect_err("rôle refusé");
        assert!(err.contains("role"));
        assert!(oai_to_anthropic(&json!({"messages": []})).is_err());
        assert!(oai_to_anthropic(&json!({"messages": [{"content": "x"}]})).is_err());
        // Valeurs « fausses » à la Python : considérées comme absentes.
        for falsy in [
            json!(null),
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            let body = json!({"messages": user, "tools": falsy, "functions": falsy,
                              "tool_choice": falsy});
            assert!(oai_to_anthropic(&body).is_ok(), "{falsy}");
        }
        assert!(oai_to_anthropic(&json!({"messages": user, "tool_choice": "auto"})).is_err());
    }

    #[test]
    fn response_translation_and_finish_reasons() {
        let out = anthropic_to_oai(&json!({
            "id": "msg_01", "model": "claude-sonnet-5", "stop_reason": "max_tokens",
            "content": [{"type": "text", "text": "hello "}, {"type": "text", "text": "world"}],
            "usage": {"input_tokens": 10, "output_tokens": 5,
                      "cache_read_input_tokens": 100, "cache_creation_input_tokens": 20},
        }));
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["id"], "msg_01");
        assert_eq!(out["choices"][0]["message"]["content"], "hello world");
        assert_eq!(out["choices"][0]["finish_reason"], "length");
        assert_eq!(
            out["usage"],
            json!({"prompt_tokens": 130, "completion_tokens": 5, "total_tokens": 135})
        );
        let bare = anthropic_to_oai(&json!({"stop_reason": "end_turn"}));
        assert_eq!(bare["choices"][0]["finish_reason"], "stop");
        assert_eq!(bare["id"], DEFAULT_ID);
        assert!(bare["created"].as_u64().is_some_and(|t| t > 1_700_000_000));
        for (reason, expected) in [("stop_sequence", "stop"), ("refusal", "stop")] {
            let out = anthropic_to_oai(&json!({"stop_reason": reason}));
            assert_eq!(out["choices"][0]["finish_reason"], expected);
        }
    }

    const EVENTS: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_9\",\"model\":\"claude-sonnet-5\",",
        "\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":3}}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"He\"}}\n",
        "\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"llo\"}}\n",
        "\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
        "\"usage\":{\"output_tokens\":7}}\n",
        "\n",
        "data: {\"type\":\"message_stop\"}\n",
        "\n",
    );

    fn run(stream: &[u8], size: usize) -> (String, SseToOai) {
        let mut tr = SseToOai::new();
        let mut got: String = stream.chunks(size).map(|c| tr.feed(c)).collect();
        got.push_str(&tr.tail());
        (got, tr)
    }

    fn parse_chunks(s: &str) -> Vec<Value> {
        s.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter(|p| p.trim() != "[DONE]")
            .map(|p| serde_json::from_str(p).expect("chunk JSON"))
            .collect()
    }

    fn streamed_text(chunks: &[Value]) -> String {
        chunks
            .iter()
            .filter_map(|c| c["choices"].get(0))
            .filter_map(|c| c["delta"]["content"].as_str())
            .collect()
    }

    #[test]
    fn sse_translation_survives_arbitrary_chunk_boundaries() {
        for size in [1, 3, 7, EVENTS.len()] {
            let (got, tr) = run(EVENTS.as_bytes(), size);
            let chunks = parse_chunks(&got);
            assert_eq!(
                chunks[0]["choices"][0]["delta"],
                json!({"role": "assistant", "content": ""})
            );
            assert_eq!(streamed_text(&chunks), "Hello");
            assert_eq!(chunks[0]["id"], "msg_9");
            assert_eq!(chunks[0]["model"], "claude-sonnet-5");
            let finals: Vec<&Value> = chunks
                .iter()
                .filter(|c| {
                    c["choices"]
                        .get(0)
                        .is_some_and(|ch| !ch["finish_reason"].is_null())
                })
                .collect();
            assert_eq!(
                finals.last().expect("chunk final")["choices"][0]["finish_reason"],
                "stop"
            );
            let usage = &chunks
                .iter()
                .rfind(|c| c["choices"].as_array().is_some_and(Vec::is_empty))
                .expect("chunk d'usage")["usage"];
            assert_eq!(
                *usage,
                json!({"prompt_tokens": 15, "completion_tokens": 7, "total_tokens": 22})
            );
            assert!(got.trim_end().ends_with("data: [DONE]"));
            let u = tr.usage().expect("usage cumulé");
            assert_eq!(
                (u.input_tokens, u.output_tokens, u.cache_read_input_tokens),
                (12, 7, 3)
            );
        }
    }

    #[test]
    fn sse_multibyte_text_survives_byte_by_byte_feeding() {
        let stream = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\r\n\r\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"café \"}}\r\n\r\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"🦀✓\"}}\r\n\r\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\r\n\r\n",
        );
        let (got, tr) = run(stream.as_bytes(), 1);
        let chunks = parse_chunks(&got);
        assert_eq!(streamed_text(&chunks), "café 🦀✓");
        assert!(got.contains("length"));
        assert_eq!(tr.usage(), None);
    }

    #[test]
    fn tail_without_events_still_closes_the_stream() {
        let tr = SseToOai::default();
        let chunks = parse_chunks(&tr.tail());
        assert_eq!(chunks[0]["id"], DEFAULT_ID);
        assert_eq!(chunks[0]["choices"][0]["finish_reason"], "stop");
        assert_eq!(chunks[1]["usage"]["total_tokens"], 0);
        assert!(tr.tail().ends_with("data: [DONE]\n\n"));
    }
}
