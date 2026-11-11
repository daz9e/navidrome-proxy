//! Request parameters of a Subsonic call.
//!
//! Clients send parameters in the query string or, with the OpenSubsonic `formPost`
//! extension, as an urlencoded POST body. Several parameters repeat (`id` in
//! `star`, `songIdToAdd` in `updatePlaylist`), so they are kept as a multi-map.

use regex::Regex;
use std::collections::HashMap;
use std::fmt;
use std::sync::LazyLock;

pub const CREDENTIAL_KEYS: [&str; 5] = ["u", "p", "t", "s", "apiKey"];

/// An ordered multi-map of request parameters.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Params(Vec<(String, String)>);

/// Subsonic's `enc:<hex>` value encoding; the password stays as sent (Navidrome decodes it).
fn decode_enc(key: &str, value: String) -> String {
    if key == "p" {
        return value;
    }
    match value.strip_prefix("enc:").and_then(|h| hex::decode(h).ok()) {
        Some(bytes) => String::from_utf8(bytes).unwrap_or(value),
        None => value,
    }
}

impl Params {
    pub fn new<K: Into<String>, V: Into<String>>(items: impl IntoIterator<Item = (K, V)>) -> Self {
        Self(items.into_iter().map(|(k, v)| (k.into(), v.into())).collect())
    }

    pub fn parse(query: &str, body: &str) -> Self {
        let items = url::form_urlencoded::parse(query.as_bytes())
            .chain(url::form_urlencoded::parse(body.as_bytes()))
            .map(|(k, v)| {
                let v = decode_enc(&k, v.into_owned());
                (k.into_owned(), v)
            });
        Self(items.collect())
    }

    pub fn get(&self, key: &str) -> &str {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or("")
    }

    pub fn get_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or(default)
    }

    pub fn getall(&self, key: &str) -> Vec<String> {
        self.0.iter().filter(|(k, _)| k == key).map(|(_, v)| v.clone()).collect()
    }

    pub fn int(&self, key: &str, default: i64) -> i64 {
        if !self.contains(key) {
            return default;
        }
        self.get(key).trim().parse().unwrap_or(default)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }

    pub fn items(&self) -> &[(String, String)] {
        &self.0
    }

    /// A copy where every `key` is replaced by `values` (possibly none), at the first position.
    pub fn replace<S: Into<String>>(&self, key: &str, values: impl IntoIterator<Item = S>) -> Self {
        let values: Vec<(String, String)> = values.into_iter().map(|v| (key.to_string(), v.into())).collect();
        let mut out = Vec::with_capacity(self.0.len() + values.len());
        let mut pending = Some(values);
        for (k, v) in &self.0 {
            if k == key {
                if let Some(vals) = pending.take() {
                    out.extend(vals);
                }
                continue;
            }
            out.push((k.clone(), v.clone()));
        }
        if let Some(vals) = pending {
            out.extend(vals);
        }
        Self(out)
    }

    pub fn without(&self, keys: &[&str]) -> Self {
        Self(self.0.iter().filter(|(k, _)| !keys.contains(&k.as_str())).cloned().collect())
    }

    pub fn with(&self, key: &str, value: impl Into<String>) -> Self {
        self.replace(key, [value.into()])
    }

    /// Only the auth/protocol parameters, for calls the proxy makes itself.
    pub fn credentials(&self) -> Self {
        Self(
            self.0
                .iter()
                .filter(|(k, _)| CREDENTIAL_KEYS.contains(&k.as_str()) || k == "v" || k == "c")
                .cloned()
                .collect(),
        )
    }

    pub fn encode(&self) -> String {
        url::form_urlencoded::Serializer::new(String::new()).extend_pairs(&self.0).finish()
    }

    /// First value of every key (what response rendering needs: `f`, `callback`).
    pub fn simple(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for (k, v) in &self.0 {
            out.entry(k.clone()).or_insert_with(|| v.clone());
        }
        out
    }

    pub fn username(&self) -> &str {
        self.get("u")
    }

    pub fn has_credentials(&self) -> bool {
        (!self.get("u").is_empty()
            && (!self.get("p").is_empty() || (!self.get("t").is_empty() && !self.get("s").is_empty())))
            || !self.get("apiKey").is_empty()
    }
}

impl fmt::Debug for Params {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Params({})", redact(&self.encode()))
    }
}

static REDACT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)([?&](?:p|t|s|apiKey)=)[^&\s"]*"#).unwrap());

/// Hide secrets in anything that is logged.
pub fn redact(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let prefixed = text.starts_with(['?', '&']);
    let input = if prefixed { text.to_string() } else { format!("&{text}") };
    let cleaned = REDACT.replace_all(&input, "${1}***").into_owned();
    if prefixed { cleaned } else { cleaned[1..].to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_and_form_body_are_merged_in_order() {
        let p = Params::parse("u=alice&id=1&id=2&f=json", "id=3&v=1.16.1");
        assert_eq!(p.getall("id"), ["1", "2", "3"]);
        assert_eq!(p.get("v"), "1.16.1");
        assert_eq!(p.username(), "alice");
    }

    #[test]
    fn replace_keeps_position_and_can_drop() {
        let p = Params::parse("u=a&id=deezer:1&id=L2&f=json", "");
        assert_eq!(p.replace("id", ["L1", "L2"]).encode(), "u=a&id=L1&id=L2&f=json");
        assert_eq!(p.replace("id", Vec::<String>::new()).encode(), "u=a&f=json");
        assert_eq!(p.replace("albumId", ["X"]).getall("albumId"), ["X"]);
    }

    #[test]
    fn credentials_only_keeps_auth_and_protocol() {
        let p = Params::parse("u=a&t=tok&s=salt&v=1.16.1&c=Arpeggi&f=json&id=1", "");
        let c = p.credentials();
        assert_eq!(c.encode(), "u=a&t=tok&s=salt&v=1.16.1&c=Arpeggi");
        assert!(p.has_credentials());
        assert!(!Params::parse("u=a&t=tok", "").has_credentials());
        assert!(Params::parse("apiKey=k", "").has_credentials());
    }

    #[test]
    fn int_parsing_falls_back() {
        let p = Params::parse("songCount=50&songOffset=abc", "");
        assert_eq!(p.int("songCount", 20), 50);
        assert_eq!(p.int("songOffset", 0), 0);
        assert_eq!(p.int("missing", 7), 7);
    }

    #[test]
    fn redact_hides_secrets_everywhere() {
        assert_eq!(
            redact("u=alice&p=hunter2&t=abc&s=salt&apiKey=k"),
            "u=alice&p=***&t=***&s=***&apiKey=***"
        );
        assert_eq!(redact("/rest/ping.view?p=secret&u=x"), "/rest/ping.view?p=***&u=x");
        assert!(!format!("{:?}", Params::parse("u=a&p=secret", "")).contains("secret"));
    }

    #[test]
    fn decodes_plus_percent_and_enc_values() {
        let id = hex::encode("deezer:5");
        let p = Params::parse(&format!("query=a+b%26c&id=enc:{id}&p=enc:6869"), "");
        assert_eq!(p.get("query"), "a b&c");
        assert_eq!(p.get("id"), "deezer:5");
        assert_eq!(p.get("p"), "enc:6869", "the password is relayed as sent");
    }
}
