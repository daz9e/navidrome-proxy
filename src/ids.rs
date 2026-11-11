//! Virtual identifiers for entities that exist outside the local Navidrome library.
//!
//! The scheme is kept identical to the first proxy generation so client caches,
//! queued likes and stored mappings survive the rewrite:
//!
//! * `deezer:<track id>`         – a Deezer track
//! * `deezer-album:<album id>`   – a Deezer album
//! * `deezer-artist:<artist id>` – a Deezer artist
//! * `yt:<video id>`             – a YouTube video used as a standalone track
//! * `sc:<track id>`             – a SoundCloud track (not in Deezer/YouTube Music)

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Track,
    Album,
    Artist,
    Youtube,
    Soundcloud,
}

// "deezer-album:" and "deezer-artist:" must be tested before "deezer:".
const PREFIXES: [(&str, Kind); 5] = [
    ("deezer-album:", Kind::Album),
    ("deezer-artist:", Kind::Artist),
    ("deezer:", Kind::Track),
    ("yt:", Kind::Youtube),
    ("sc:", Kind::Soundcloud),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualId {
    pub kind: Kind,
    pub key: String,
}

impl VirtualId {
    pub fn is_track(&self) -> bool {
        matches!(self.kind, Kind::Track | Kind::Youtube | Kind::Soundcloud)
    }
}

impl fmt::Display for VirtualId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = PREFIXES
            .iter()
            .find(|(_, k)| *k == self.kind)
            .map(|(p, _)| *p)
            .unwrap_or("");
        write!(f, "{prefix}{}", self.key)
    }
}

pub fn valid_youtube_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn numeric(key: &str) -> bool {
    (1..=20).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_digit())
}

/// The virtual id encoded in `value`, or `None` for a Navidrome id.
pub fn parse(value: &str) -> Option<VirtualId> {
    let (prefix, kind) = PREFIXES.iter().find(|(p, _)| value.starts_with(p))?;
    let key = &value[prefix.len()..];
    let valid = if *kind == Kind::Youtube {
        valid_youtube_id(key)
    } else {
        numeric(key)
    };
    valid.then(|| VirtualId {
        kind: *kind,
        key: key.to_string(),
    })
}

pub fn is_virtual(value: &str) -> bool {
    parse(value).is_some()
}

pub fn track(deezer_id: impl fmt::Display) -> String {
    format!("deezer:{deezer_id}")
}

pub fn album(deezer_id: impl fmt::Display) -> String {
    format!("deezer-album:{deezer_id}")
}

pub fn artist(deezer_id: impl fmt::Display) -> String {
    format!("deezer-artist:{deezer_id}")
}

pub fn youtube(video_id: &str) -> String {
    format!("yt:{video_id}")
}

pub fn soundcloud(track_id: impl fmt::Display) -> String {
    format!("sc:{track_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_kind() {
        assert_eq!(parse("deezer:123").unwrap().kind, Kind::Track);
        assert_eq!(parse("deezer-album:5").unwrap().kind, Kind::Album);
        assert_eq!(parse("deezer-artist:5").unwrap().kind, Kind::Artist);
        assert_eq!(parse("yt:dQw4w9WgXcQ").unwrap().kind, Kind::Youtube);
        assert_eq!(parse("sc:42").unwrap().kind, Kind::Soundcloud);
        assert!(parse("sc:42").unwrap().is_track());
        assert!(!parse("deezer-album:5").unwrap().is_track());
    }

    #[test]
    fn rejects_malformed_and_local_ids() {
        for bad in [
            "",
            "deezer:",
            "deezer:abc",
            "yt:short",
            "yt:dQw4w9WgXcQx",
            "4f2c9a",
            "deezer:1/../2",
        ] {
            assert!(parse(bad).is_none(), "{bad}");
        }
        assert!(parse(&format!("deezer:{}", "1".repeat(21))).is_none());
    }

    #[test]
    fn round_trips() {
        for id in [
            "deezer:1",
            "deezer-album:2",
            "deezer-artist:3",
            "yt:dQw4w9WgXcQ",
            "sc:4",
        ] {
            assert_eq!(parse(id).unwrap().to_string(), id);
        }
        assert_eq!(track(7), "deezer:7");
        assert_eq!(youtube("dQw4w9WgXcQ"), "yt:dQw4w9WgXcQ");
    }
}
