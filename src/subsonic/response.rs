//! Subsonic response envelopes in JSON and XML.
//!
//! Navidrome answers every API error with HTTP 200 and `status="failed"`; the proxy
//! does the same so strict clients (Swift `Codable` decoders in Arpeggi) see exactly
//! the shapes they already handle.

use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;

pub const XMLNS: &str = "http://subsonic.org/restapi";

// Error codes from the Subsonic API specification.
pub const GENERIC: i64 = 0;
pub const MISSING_PARAMETER: i64 = 10;
pub const WRONG_CREDENTIALS: i64 = 40;
pub const NOT_AUTHORIZED: i64 = 50;
pub const NOT_FOUND: i64 = 70;

/// What the proxy claims to be; refreshed from the upstream `ping`.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerIdentity {
    pub version: String,
    pub kind: String,
    pub server_version: String,
    pub open_subsonic: bool,
}

impl Default for ServerIdentity {
    fn default() -> Self {
        Self {
            version: "1.16.1".into(),
            kind: "navidrome".into(),
            server_version: "0.63.2".into(),
            open_subsonic: true,
        }
    }
}

impl ServerIdentity {
    pub fn update_from(&mut self, root: &Map<String, Value>) {
        let text = |key: &str, cur: &str| match root.get(key) {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => cur.to_string(),
        };
        self.version = text("version", &self.version);
        self.kind = text("type", &self.kind);
        self.server_version = text("serverVersion", &self.server_version);
        if let Some(v) = root.get("openSubsonic") {
            self.open_subsonic = crate::util::truthy(Some(v));
        }
    }
}

pub fn wants_xml(params: &HashMap<String, String>) -> bool {
    !matches!(params.get("f").map(|f| f.to_lowercase()).as_deref(), Some("json" | "jsonp"))
}

pub fn envelope(identity: &ServerIdentity, payload: Option<Map<String, Value>>, status: &str) -> Value {
    let mut root = Map::new();
    root.insert("status".into(), json!(status));
    root.insert("version".into(), json!(identity.version));
    root.insert("type".into(), json!(identity.kind));
    root.insert("serverVersion".into(), json!(identity.server_version));
    root.insert("openSubsonic".into(), json!(identity.open_subsonic));
    if let Some(payload) = payload {
        root.extend(payload);
    }
    json!({ "subsonic-response": root })
}

fn xml_scalar(value: &Value) -> String {
    match value {
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn escape(text: &str, attribute: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            '\n' if attribute => out.push_str("&#10;"),
            '\r' if attribute => out.push_str("&#13;"),
            '\t' if attribute => out.push_str("&#09;"),
            // Characters XML 1.0 cannot carry at all.
            c if (c as u32) < 0x20 && !matches!(c, '\n' | '\r' | '\t') => {}
            c => out.push(c),
        }
    }
    out
}

struct Element {
    tag: String,
    attrs: Vec<(String, String)>,
    text: Option<String>,
    children: Vec<Element>,
}

impl Element {
    fn new(tag: &str) -> Self {
        Self { tag: tag.into(), attrs: Vec::new(), text: None, children: Vec::new() }
    }

    fn fill(&mut self, payload: &Map<String, Value>) {
        for (key, value) in payload {
            match value {
                Value::Null => {}
                Value::Object(map) => {
                    let mut child = Element::new(key);
                    child.fill(map);
                    self.children.push(child);
                }
                Value::Array(items) => {
                    for item in items {
                        let mut child = Element::new(key);
                        match item {
                            Value::Object(map) => child.fill(map),
                            other => child.text = Some(xml_scalar(other)),
                        }
                        self.children.push(child);
                    }
                }
                other if key == "value" && matches!(self.tag.as_str(), "line" | "biography") => {
                    self.text = Some(xml_scalar(other));
                }
                other => self.attrs.push((key.clone(), xml_scalar(other))),
            }
        }
    }

    fn write(&self, out: &mut String) {
        out.push('<');
        out.push_str(&self.tag);
        for (k, v) in &self.attrs {
            out.push_str(&format!(" {k}=\"{}\"", escape(v, true)));
        }
        if self.text.is_none() && self.children.is_empty() {
            out.push_str(" />");
            return;
        }
        out.push('>');
        if let Some(text) = &self.text {
            out.push_str(&escape(text, false));
        }
        for child in &self.children {
            child.write(out);
        }
        out.push_str(&format!("</{}>", self.tag));
    }
}

pub fn to_xml(document: &Value) -> Vec<u8> {
    let mut root = Element::new("subsonic-response");
    root.attrs.push(("xmlns".into(), XMLNS.into()));
    if let Some(payload) = document.get("subsonic-response").and_then(Value::as_object) {
        root.fill(payload);
    }
    let mut out = String::from("<?xml version='1.0' encoding='utf-8'?>\n");
    root.write(&mut out);
    out.into_bytes()
}

static CALLBACK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_$][A-Za-z0-9_.$]{0,127}$").unwrap());

/// Serialize `document` in the format the client asked for.
pub fn render(params: &HashMap<String, String>, document: &Value) -> (Vec<u8>, &'static str) {
    let fmt = params.get("f").map(|f| f.to_lowercase()).unwrap_or_else(|| "xml".into());
    let body = || serde_json::to_vec(document).unwrap_or_default();
    // An arbitrary callback would be reflected as executable script; only plain names are honoured.
    if let Some(cb) = params.get("callback").filter(|cb| fmt == "jsonp" && CALLBACK.is_match(cb)) {
        let mut out = format!("{cb}(").into_bytes();
        out.extend(body());
        out.extend(b");");
        return (out, "application/javascript; charset=utf-8");
    }
    if fmt == "json" || fmt == "jsonp" {
        return (body(), "application/json; charset=utf-8");
    }
    (to_xml(document), "application/xml; charset=utf-8")
}

pub fn ok(
    identity: &ServerIdentity,
    params: &HashMap<String, String>,
    payload: Option<Map<String, Value>>,
) -> (Vec<u8>, &'static str) {
    render(params, &envelope(identity, payload, "ok"))
}

pub fn error(
    identity: &ServerIdentity,
    params: &HashMap<String, String>,
    code: i64,
    message: &str,
) -> (Vec<u8>, &'static str) {
    let mut payload = Map::new();
    payload.insert("error".into(), json!({"code": code, "message": message}));
    render(params, &envelope(identity, Some(payload), "failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ServerIdentity {
        ServerIdentity { server_version: "0.63.2 (be10f89c)".into(), ..Default::default() }
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn payload(v: Value) -> Option<Map<String, Value>> {
        v.as_object().cloned()
    }

    #[test]
    fn json_envelope_matches_navidrome_shape() {
        let (body, ctype) =
            ok(&identity(), &params(&[("f", "json")]), payload(json!({"song": {"id": "x", "isDir": false}})));
        assert!(ctype.starts_with("application/json"));
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v["subsonic-response"],
            json!({
                "status": "ok", "version": "1.16.1", "type": "navidrome",
                "serverVersion": "0.63.2 (be10f89c)", "openSubsonic": true,
                "song": {"id": "x", "isDir": false}
            })
        );
        // Key order is Navidrome's: status first.
        assert!(String::from_utf8(body).unwrap().starts_with(r#"{"subsonic-response":{"status":"ok""#));
    }

    #[test]
    fn xml_is_the_default_and_nests_lists_and_objects() {
        let p = payload(json!({"searchResult3": {"song": [
            {"id": "1", "isDir": false, "genres": [{"name": "Rap"}], "replayGain": {}, "isrc": ["A"], "title": "a<b&\"c\""},
            {"id": "2", "isDir": false, "x": null}
        ]}}));
        let (body, ctype) = ok(&identity(), &HashMap::new(), p);
        assert!(ctype.starts_with("application/xml"));
        let xml = String::from_utf8(body).unwrap();
        assert!(xml.starts_with("<?xml version='1.0' encoding='utf-8'?>\n<subsonic-response xmlns=\"http://subsonic.org/restapi\" status=\"ok\""));
        assert!(xml.contains("openSubsonic=\"true\""));
        assert!(xml.contains("<song id=\"1\" isDir=\"false\" title=\"a&lt;b&amp;&quot;c&quot;\"><genres name=\"Rap\" /><replayGain /><isrc>A</isrc></song>"));
        assert!(xml.contains("<song id=\"2\" isDir=\"false\" />"));
    }

    #[test]
    fn lyrics_lines_are_text() {
        let p = payload(json!({"lyricsList": {"structuredLyrics": [{"line": [{"start": 0, "value": "la & la"}]}]}}));
        let xml = String::from_utf8(ok(&identity(), &HashMap::new(), p).0).unwrap();
        assert!(xml.contains("<line start=\"0\">la &amp; la</line>"));
    }

    #[test]
    fn error_is_status_failed_with_code() {
        let (body, _) = error(&identity(), &params(&[("f", "json")]), NOT_FOUND, "Song not found");
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["subsonic-response"]["status"], "failed");
        assert_eq!(v["subsonic-response"]["error"], json!({"code": 70, "message": "Song not found"}));
    }

    #[test]
    fn jsonp_wraps_only_safe_callbacks() {
        let (body, ctype) = ok(&identity(), &params(&[("f", "jsonp"), ("callback", "cb")]), None);
        assert!(body.starts_with(b"cb(") && body.ends_with(b");"));
        assert!(ctype.contains("javascript"));
        let (body, ctype) = ok(&identity(), &params(&[("f", "jsonp"), ("callback", "alert(1)//")]), None);
        assert!(body.starts_with(b"{"));
        assert!(ctype.starts_with("application/json"));
    }

    #[test]
    fn identity_updates_from_upstream_ping() {
        let mut id = ServerIdentity::default();
        id.update_from(json!({"version": "1.16.1", "type": "navidrome", "serverVersion": "0.64.0", "openSubsonic": true}).as_object().unwrap());
        assert_eq!(id.server_version, "0.64.0");
        assert!(wants_xml(&HashMap::new()));
        assert!(!wants_xml(&params(&[("f", "JSON")])));
    }
}
