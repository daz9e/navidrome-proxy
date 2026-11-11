//! Runtime settings, read once from the environment.

use std::collections::HashMap;
use std::path::PathBuf;

/// All tunables of the proxy.
///
/// Paths are the paths *inside* the proxy's own filesystem; the music directory
/// must be the same directory Navidrome scans (`ND_MUSICFOLDER`).
#[derive(Debug, Clone)]
pub struct Settings {
    pub host: String,
    pub port: u16,
    pub navidrome_url: String,
    /// Optional Navidrome admin account for background work (startScan needs admin;
    /// without it the proxy uses any credentials it recently saw and relies on
    /// Navidrome's file watcher).
    pub navidrome_user: String,
    pub navidrome_password: String,
    pub music_dir: PathBuf,
    pub data_dir: PathBuf,
    pub slskd_url: String,
    pub slskd_api_key: String,
    pub slskd_downloads_dir: PathBuf,

    pub background_workers: usize,
    pub priority_workers: usize,
    pub stream_wait_seconds: f64,
    pub auth_cache_seconds: f64,
    pub search_cache_seconds: f64,
    pub search_budget_seconds: f64,
    pub deezer_requests_per_second: f64,
    pub scan_debounce_seconds: f64,
    pub duration_tolerance_seconds: f64,

    pub external_songs: usize,
    pub external_albums: usize,
    pub external_artists: usize,
    pub youtube_songs: usize,

    pub complete_local_albums: bool,
    pub prefetch_next_tracks: usize,
    pub slskd_upgrades: bool,
    pub slskd_min_peers: usize,
    pub lyrics: bool,
    pub backup_hours: f64,
    pub backup_keep: usize,
    pub log_level: String,

    pub ytdlp_path: String,
    /// Extra arguments for every yt-dlp call (e.g. `--extractor-args youtube:player_client=...`).
    pub ytdlp_args: Vec<String>,
    pub ffmpeg_path: String,
    pub ffprobe_path: String,
    pub fpcalc_path: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8084,
            navidrome_url: "http://127.0.0.1:4533".into(),
            navidrome_user: String::new(),
            navidrome_password: String::new(),
            music_dir: "/music".into(),
            data_dir: "/data".into(),
            slskd_url: String::new(),
            slskd_api_key: String::new(),
            slskd_downloads_dir: "/downloads-slskd".into(),
            background_workers: 5,
            priority_workers: 16,
            stream_wait_seconds: 120.0,
            auth_cache_seconds: 300.0,
            search_cache_seconds: 120.0,
            search_budget_seconds: 4.0,
            deezer_requests_per_second: 8.0,
            scan_debounce_seconds: 3.0,
            duration_tolerance_seconds: 10.0,
            external_songs: 25,
            external_albums: 10,
            external_artists: 5,
            youtube_songs: 3,
            complete_local_albums: true,
            prefetch_next_tracks: 2,
            slskd_upgrades: true,
            slskd_min_peers: 2,
            lyrics: true,
            backup_hours: 6.0,
            backup_keep: 8,
            log_level: "INFO".into(),
            ytdlp_path: "yt-dlp".into(),
            ytdlp_args: Vec::new(),
            ffmpeg_path: "ffmpeg".into(),
            ffprobe_path: "ffprobe".into(),
            fpcalc_path: "fpcalc".into(),
        }
    }
}

fn parse_bool(value: Option<&String>, default: bool) -> bool {
    match value.map(|v| v.trim()) {
        None | Some("") => default,
        Some(v) => matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"),
    }
}

fn parse_num<T: std::str::FromStr>(value: Option<&String>, default: T) -> T {
    value.and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

impl Settings {
    pub fn from_process_env() -> Self {
        Self::from_env(&std::env::vars().collect())
    }

    pub fn from_env(e: &HashMap<String, String>) -> Self {
        let d = Self::default();
        let s = |key: &str, default: &str| e.get(key).cloned().unwrap_or_else(|| default.to_string());
        // `<NAME>_FILE` holds a secret on disk (systemd credentials, agenix, sops).
        let secret = |key: &str| {
            e.get(&format!("{key}_FILE"))
                .filter(|p| !p.is_empty())
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|v| v.trim().to_string())
                .unwrap_or_else(|| s(key, ""))
        };
        Self {
            host: s("HOST", &d.host),
            port: parse_num(e.get("PORT"), d.port),
            navidrome_url: s("NAVIDROME_URL", &d.navidrome_url).trim_end_matches('/').to_string(),
            navidrome_user: s("NAVIDROME_USER", ""),
            navidrome_password: secret("NAVIDROME_PASSWORD"),
            music_dir: s("MUSIC_DIR", "/music").into(),
            data_dir: s("DATA_DIR", "/data").into(),
            slskd_url: s("SLSKD_URL", "").trim_end_matches('/').to_string(),
            slskd_api_key: secret("SLSKD_API_KEY"),
            slskd_downloads_dir: s("SLSKD_DOWNLOADS_DIR", "/downloads-slskd").into(),
            background_workers: parse_num(e.get("BACKGROUND_WORKERS"), d.background_workers).max(1),
            priority_workers: parse_num(e.get("PRIORITY_WORKERS"), d.priority_workers).max(1),
            stream_wait_seconds: parse_num(e.get("STREAM_WAIT_SECONDS"), d.stream_wait_seconds),
            auth_cache_seconds: parse_num(e.get("AUTH_CACHE_SECONDS"), d.auth_cache_seconds),
            search_cache_seconds: parse_num(e.get("SEARCH_CACHE_SECONDS"), d.search_cache_seconds),
            search_budget_seconds: parse_num(e.get("SEARCH_BUDGET_SECONDS"), d.search_budget_seconds),
            deezer_requests_per_second: parse_num(
                e.get("DEEZER_REQUESTS_PER_SECOND"),
                d.deezer_requests_per_second,
            ),
            scan_debounce_seconds: parse_num(e.get("SCAN_DEBOUNCE_SECONDS"), d.scan_debounce_seconds),
            duration_tolerance_seconds: parse_num(
                e.get("DURATION_TOLERANCE_SECONDS"),
                d.duration_tolerance_seconds,
            ),
            external_songs: parse_num(e.get("EXTERNAL_SONGS"), d.external_songs),
            external_albums: parse_num(e.get("EXTERNAL_ALBUMS"), d.external_albums),
            external_artists: parse_num(e.get("EXTERNAL_ARTISTS"), d.external_artists),
            youtube_songs: parse_num(e.get("YOUTUBE_SONGS"), d.youtube_songs),
            complete_local_albums: parse_bool(e.get("COMPLETE_LOCAL_ALBUMS"), d.complete_local_albums),
            prefetch_next_tracks: parse_num(e.get("PREFETCH_NEXT_TRACKS"), d.prefetch_next_tracks),
            slskd_upgrades: parse_bool(e.get("SLSKD_UPGRADES"), d.slskd_upgrades),
            slskd_min_peers: parse_num(e.get("SLSKD_MIN_PEERS"), d.slskd_min_peers).max(1),
            lyrics: parse_bool(e.get("LYRICS"), d.lyrics),
            backup_hours: parse_num(e.get("BACKUP_HOURS"), d.backup_hours),
            backup_keep: parse_num(e.get("BACKUP_KEEP"), d.backup_keep).max(1),
            log_level: s("LOG_LEVEL", &d.log_level).to_uppercase(),
            ytdlp_path: s("YTDLP_PATH", &d.ytdlp_path),
            ytdlp_args: s("YTDLP_ARGS", "").split_whitespace().map(String::from).collect(),
            ffmpeg_path: s("FFMPEG_PATH", &d.ffmpeg_path),
            ffprobe_path: s("FFPROBE_PATH", &d.ffprobe_path),
            fpcalc_path: s("FPCALC_PATH", &d.fpcalc_path),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("proxy.sqlite3")
    }

    /// Staging must live on the same filesystem as the music directory so the
    /// final publish is an atomic rename.
    pub fn staging_dir(&self) -> PathBuf {
        self.music_dir.join(".navidrome-proxy-staging")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.data_dir.join("cache")
    }

    pub fn backup_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }

    pub fn slskd_enabled(&self) -> bool {
        !self.slskd_url.is_empty() && !self.slskd_api_key.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn defaults() {
        let s = Settings::from_env(&HashMap::new());
        assert_eq!(s.port, 8084);
        assert_eq!(s.navidrome_url, "http://127.0.0.1:4533");
        assert_eq!(s.db_path(), PathBuf::from("/data/proxy.sqlite3"));
        assert_eq!(s.staging_dir(), PathBuf::from("/music/.navidrome-proxy-staging"));
        assert!(!s.slskd_enabled());
    }

    #[test]
    fn parses_and_clamps() {
        let s = Settings::from_env(&env(&[
            ("PORT", "9000"),
            ("NAVIDROME_URL", "http://nd:4533/"),
            ("BACKGROUND_WORKERS", "0"),
            ("STREAM_WAIT_SECONDS", "oops"),
            ("LYRICS", "off"),
            ("COMPLETE_LOCAL_ALBUMS", ""),
            ("SLSKD_URL", "http://s/"),
            ("SLSKD_API_KEY", "k"),
            ("LOG_LEVEL", "debug"),
            ("YTDLP_ARGS", " --a  b "),
        ]));
        assert_eq!(s.port, 9000);
        assert_eq!(s.navidrome_url, "http://nd:4533");
        assert_eq!(s.background_workers, 1);
        assert_eq!(s.stream_wait_seconds, 120.0);
        assert!(!s.lyrics);
        assert!(s.complete_local_albums);
        assert!(s.slskd_enabled());
        assert_eq!(s.slskd_url, "http://s");
        assert_eq!(s.log_level, "DEBUG");
        assert_eq!(s.ytdlp_args, vec!["--a", "b"]);
    }

    #[test]
    fn secrets_from_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pw");
        std::fs::write(&path, "hunter2\n").unwrap();
        let s = Settings::from_env(&env(&[
            ("NAVIDROME_PASSWORD", "ignored"),
            ("NAVIDROME_PASSWORD_FILE", path.to_str().unwrap()),
        ]));
        assert_eq!(s.navidrome_password, "hunter2");
    }
}
