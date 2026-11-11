//! Subsonic endpoints the proxy answers itself (everything else is relayed).

pub mod browse;
pub mod likes;
pub mod playback;
pub mod playlists;
pub mod search;

use crate::app::Services;
use crate::navidrome::{Incoming, NdError};
use crate::subsonic::params::Params;
use crate::subsonic::response as sr;
use crate::util::Json;
use axum::http::{HeaderMap, Method, header};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// One Subsonic API call.
pub struct Call {
    pub method: String,
    pub params: Params,
    pub http_method: Method,
    pub headers: HeaderMap,
    pub remote: Option<std::net::IpAddr>,
}

/// A handler's answer: a response, or None to pass the request through unchanged.
pub type Reply = Result<Option<Response>, NdError>;

impl Call {
    pub fn username(&self) -> &str {
        self.params.username()
    }

    pub fn is_json(&self) -> bool {
        matches!(self.params.get_or("f", "xml").to_lowercase().as_str(), "json" | "jsonp")
    }

    pub fn creds(&self) -> Params {
        self.params.credentials()
    }

    pub fn ok(&self, s: &Services, payload: Option<Json>) -> Response {
        let (body, ctype) = sr::ok(&s.navidrome.identity(), &self.params.simple(), payload);
        ([(header::CONTENT_TYPE, ctype)], body).into_response()
    }

    pub fn ok_with(&self, s: &Services, key: &str, value: serde_json::Value) -> Response {
        let mut payload = Json::new();
        payload.insert(key.into(), value);
        self.ok(s, Some(payload))
    }

    pub fn error(&self, s: &Services, code: i64, message: &str) -> Response {
        let (body, ctype) = sr::error(&s.navidrome.identity(), &self.params.simple(), code, message);
        ([(header::CONTENT_TYPE, ctype)], body).into_response()
    }

    pub fn not_found(&self, s: &Services, what: &str) -> Response {
        self.error(s, sr::NOT_FOUND, &format!("{what} not found"))
    }

    /// Relay this call to Navidrome as `path?query` (parameters rewritten, no body).
    pub async fn relay(&self, s: &Services, path: &str, query: &Params) -> Result<Response, NdError> {
        let target = format!("{path}?{}", query.encode());
        let incoming = Incoming { method: &self.http_method, path_and_query: &target, headers: &self.headers, remote: self.remote };
        s.navidrome.passthrough(incoming, reqwest::Body::from(Vec::new())).await
    }
}

/// Methods with a handler; everything else is relayed without authentication by the proxy.
pub const HANDLED: &[&str] = &[
    "getSong",
    "getAlbum",
    "getArtist",
    "getArtistInfo",
    "getArtistInfo2",
    "getAlbumInfo",
    "getAlbumInfo2",
    "getTopSongs",
    "getSimilarSongs",
    "getSimilarSongs2",
    "getCoverArt",
    "getLyricsBySongId",
    "star",
    "unstar",
    "setRating",
    "getStarred",
    "getStarred2",
    "stream",
    "download",
    "scrobble",
    "savePlayQueue",
    "savePlayQueueByIndex",
    "createPlaylist",
    "updatePlaylist",
    "getPlaylist",
    "search2",
    "search3",
];

pub async fn dispatch(s: &Arc<Services>, call: &Call) -> Reply {
    match call.method.as_str() {
        "getSong" => browse::get_song(s, call).await,
        "getAlbum" => browse::get_album(s, call).await,
        "getArtist" => browse::get_artist(s, call).await,
        "getArtistInfo" | "getArtistInfo2" => browse::get_artist_info(s, call).await,
        "getAlbumInfo" | "getAlbumInfo2" => browse::get_album_info(s, call).await,
        "getTopSongs" => browse::get_top_songs(s, call).await,
        "getSimilarSongs" | "getSimilarSongs2" => browse::get_similar_songs(s, call).await,
        "getCoverArt" => browse::get_cover_art(s, call).await,
        "getLyricsBySongId" => browse::get_lyrics_by_song_id(s, call).await,
        "star" | "unstar" => likes::star(s, call).await,
        "setRating" => likes::set_rating(s, call).await,
        "getStarred" | "getStarred2" => likes::get_starred(s, call).await,
        "stream" | "download" => playback::stream(s, call).await,
        "scrobble" => playback::scrobble(s, call).await,
        "savePlayQueue" => playback::save_play_queue(s, call).await,
        "savePlayQueueByIndex" => playback::save_play_queue_by_index(s, call).await,
        "createPlaylist" => playlists::create_playlist(s, call).await,
        "updatePlaylist" => playlists::update_playlist(s, call).await,
        "getPlaylist" => playlists::get_playlist(s, call).await,
        "search2" | "search3" => search::search(s, call).await,
        _ => Ok(None),
    }
}
