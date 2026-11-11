//! Where a YouTube/SoundCloud upload goes when no catalog album came with it.
//!
//! * **Official** – the catalog has this very recording (its artist named in the title
//!   or uploader, its title in the video title, same version markers, same length):
//!   tagged as that Deezer track, in its album.
//! * **Unofficial** – a known artist, but not a catalog recording (slowed, mashup, edit,
//!   leak): `<artist>/Unofficial`, under the upload's own title. The artist is the
//!   original's when the catalog knows the song, else the first one named in the title.
//! * **Custom** – nobody found: artist, album artist and album `Custom`.
//!
//! Names are *recognized* (catalog names looked up in the title words), never parsed
//! out of the title, so separators, order and alphabet do not matter.

use crate::deezer::{self, Deezer, DeezerError};
use crate::identity::DURATION_SLACK;
use crate::matching::{self, find_name, markers, search_text, segments};
use crate::util::{Json, as_f64, as_str, as_uint, opt_str};
use serde_json::{Value, json};

pub const UNOFFICIAL: &str = "Unofficial";
pub const CUSTOM: &str = "Custom";
/// shortcut: fan count is the "real artist" test for names seen only as the uploader
/// or as one short word ("sad", "les"); replace it if namesakes still slip through.
const MIN_FANS: u64 = 1000;
/// Artist searches per upload when the catalog has no original.
const MAX_ARTIST_QUERIES: usize = 10;

#[derive(Debug, Clone, PartialEq)]
pub enum Placement {
    /// Full catalog metadata of the same recording.
    Official(Json),
    /// Credited artists (the album artist first) and that artist's picture.
    Unofficial {
        artists: Vec<String>,
        picture: Option<String>,
    },
    Custom,
}

fn glued(text: &str) -> String {
    matching::latin(text).replace(' ', "")
}

/// "фортуна812" -> "fortuna 812": how the catalog spells a name typed in Cyrillic or glued.
fn latin_query(text: &str) -> String {
    static BOUNDARY: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(\p{L})(\p{N})|(\p{N})(\p{L})").unwrap());
    let latin = matching::latin(text);
    // Twice: one pass cannot split overlapping boundaries ("a1b").
    let once = BOUNDARY.replace_all(&latin, "$1$3 $2$4").into_owned();
    BOUNDARY.replace_all(&once, "$1$3 $2$4").into_owned()
}

/// Decide where an upload titled `title` by `uploader` belongs.
pub async fn place(
    deezer: &Deezer,
    title: &str,
    uploader: &str,
    duration: Option<f64>,
) -> Result<Placement, DeezerError> {
    let uploader = uploader.trim();
    let uploader = uploader
        .strip_suffix(" - Topic")
        .unwrap_or(uploader)
        .to_string();
    let named =
        |name: &str| find_name(title, name).is_some() || find_name(&uploader, name).is_some();

    // 1. The original: its main artist named somewhere, its title inside the video title.
    let mut queries = vec![search_text(title, false), search_text(title, true)];
    queries.dedup();
    for query in queries.iter().filter(|q| !q.is_empty()) {
        for row in deezer.search_tracks(query, 10, 0).await? {
            let artists = deezer::artist_names(&row);
            let name = as_str(row.get("title"));
            if !artists.first().is_some_and(|a| named(a))
                || find_name(title, &matching::base_title(&name)).is_none()
            {
                continue;
            }
            let version = format!("{name} {}", as_str(row.get("title_version")));
            let same_length = matches!(
                (duration, as_f64(row.get("duration"))),
                (Some(a), Some(b)) if a > 0.0 && b > 0.0 && (a - b).abs() <= DURATION_SLACK
            );
            if same_length && markers(&version) == markers(title) {
                let id = as_str(row.get("id"));
                let full = deezer.full_track_meta(&id).await?;
                return Ok(Placement::Official(full));
            }
            let picture = row.get("artist").and_then(|a| opt_str(a.get("picture_xl")));
            return Ok(Placement::Unofficial { artists, picture });
        }
    }

    // 2. No original: artists whose names stand in the title (or are the uploader).
    let mut queries: Vec<String> = Vec::new();
    for part in std::iter::once(uploader.clone()).chain(segments(title)) {
        for query in [matching::normalize(&part), latin_query(&part)] {
            if !query.is_empty() && !queries.contains(&query) {
                queries.push(query);
            }
        }
    }
    queries.truncate(MAX_ARTIST_QUERIES);
    let mut found: Vec<(usize, String, Option<String>)> = Vec::new();
    for query in queries {
        for row in deezer.search_artists(&query, 5).await? {
            let name = as_str(row.get("name"));
            let popular = as_uint(row.get("nb_fan")) as u64 >= MIN_FANS;
            let short = glued(&name).chars().count() < 4;
            let position = match find_name(title, &name) {
                Some(p) if popular || !short => p,
                _ if popular && !name.is_empty() && glued(&uploader) == glued(&name) => usize::MAX,
                _ => continue,
            };
            if found.iter().all(|(_, n, _)| glued(n) != glued(&name)) {
                found.push((position, name, opt_str(row.get("picture_xl"))));
            }
        }
    }
    // "FORTUNA" inside "FORTUNA 812" is the same credit: keep the longer name.
    let all: Vec<String> = found.iter().map(|(_, n, _)| n.clone()).collect();
    found.retain(|(_, n, _)| {
        !all.iter()
            .any(|o| glued(o).len() > glued(n).len() && find_name(o, n).is_some())
    });
    found.sort_by_key(|(p, ..)| *p);
    Ok(match found.first() {
        None => Placement::Custom,
        Some((_, _, picture)) => Placement::Unofficial {
            picture: picture.clone(),
            artists: found.iter().map(|(_, n, _)| n.clone()).collect(),
        },
    })
}

/// Tag metadata for `placement` on top of the upload's own `meta`.
pub fn apply(meta: &mut Json, placement: &Placement) {
    let mut credit = |artists: &[String], album: &str| {
        meta.insert("artists".into(), json!(artists));
        meta.insert("artist".into(), json!(artists.join(", ")));
        meta.insert("album_artist".into(), json!(artists[0]));
        meta.insert("album".into(), json!(album));
    };
    match placement {
        Placement::Official(full) => {
            // The upload stays a YouTube/SoundCloud track (the Deezer id becomes its alias,
            // not its album row); only its tags become the catalog's.
            for (k, v) in full {
                if !v.is_null() && !matches!(k.as_str(), "source" | "full" | "album_id") {
                    meta.insert(k.clone(), v.clone());
                }
            }
        }
        Placement::Unofficial { artists, picture } => {
            credit(artists, UNOFFICIAL);
            if let Some(p) = picture {
                meta.insert("album_cover".into(), Value::String(p.clone()));
            }
        }
        Placement::Custom => credit(&[CUSTOM.to_string()], CUSTOM),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;
    use crate::testutil::{FakeDeezer, dz_track, serve};
    use std::sync::Arc;

    async fn world(setup: impl FnOnce(&FakeDeezer)) -> (Deezer, crate::testutil::Server) {
        let fake = FakeDeezer::empty();
        setup(&fake);
        let server = serve(fake.router()).await;
        let store = Arc::new(Store::memory());
        (Deezer::new(store, 100.0, &server.url, 60.0), server)
    }

    fn artist(fake: &FakeDeezer, id: u64, name: &str, fans: u64) {
        fake.state.lock().artists.insert(
            id,
            crate::testutil::m(json!({"id": id, "name": name, "nb_fan": fans, "picture_xl": format!("https://img/{id}.jpg")})),
        );
    }

    fn fortuna(fake: &FakeDeezer) {
        let mut t = dz_track(10, "Black Water", "FORTUNA 812", 5, "Black Water", 1, &[]);
        t.insert("duration".into(), json!(150));
        fake.add_album(5, "Black Water", "FORTUNA 812", 1, vec![t]);
        artist(fake, 1, "FORTUNA 812", 20000);
        artist(fake, 2, "FORTUNA", 50);
        artist(fake, 3, "sad", 100);
        artist(fake, 4, "Elox1m", 2000);
    }

    async fn placed(title: &str, uploader: &str, duration: f64) -> Placement {
        let (dz, _server) = world(fortuna).await;
        place(&dz, title, uploader, Some(duration)).await.unwrap()
    }

    fn unofficial(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn same_recording_is_official() {
        match placed("FORTUNA 812 - Black Water (Official Audio)", "x", 151.0).await {
            Placement::Official(meta) => assert_eq!(meta["album"], "Black Water"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn versions_of_a_catalog_song_are_unofficial_under_its_artist() {
        for (title, uploader, duration) in [
            ("Black Water (slowed + reverb)", "FORTUNA 812", 190.0),
            ("фортуна⋆812 -- black water [sped up]", "someone", 120.0),
            ("black water fortuna 812", "x", 160.0), // other length: not this recording
            (
                "BLACK WATER ( EXTENDED INTRO BY VALENTINOFF )",
                "FORTUNA 812, V十",
                151.0,
            ),
        ] {
            let Placement::Unofficial { artists, picture } =
                placed(title, uploader, duration).await
            else {
                panic!("{title}");
            };
            assert_eq!(artists, unofficial(&["FORTUNA 812"]), "{title}");
            assert_eq!(picture.as_deref(), Some("https://img/1.jpg"));
        }
    }

    #[tokio::test]
    async fn unknown_songs_go_to_the_artists_named_in_the_title() {
        assert_eq!(
            placed(
                "Elox1m & FORTUNA 812 -- Порезы на руках (slowed)",
                "x",
                100.0
            )
            .await,
            Placement::Unofficial {
                artists: unofficial(&["Elox1m", "FORTUNA 812"]),
                picture: Some("https://img/4.jpg".into())
            }
        );
        // Name after the title, Cyrillic spelling, glued digits; "FORTUNA" is not a second artist.
        for title in [
            "Кошмар — FORTUNA 812",
            "Фортуна 812 - SRT",
            "СНОВа.-фортуна812",
            "фортуна⋆812 - Рассвет (prod. cloudly006)",
        ] {
            let Placement::Unofficial { artists, .. } = placed(title, "x", 100.0).await else {
                panic!("{title}");
            };
            assert_eq!(artists, unofficial(&["FORTUNA 812"]), "{title}");
        }
        // The uploader is the artist.
        let Placement::Unofficial { artists, .. } = placed("Клуб88", "FORTUNA 812", 100.0).await
        else {
            panic!();
        };
        assert_eq!(artists, unofficial(&["FORTUNA 812"]));
    }

    #[tokio::test]
    async fn nobody_found_is_custom() {
        for (title, uploader) in [
            ("crawling back to you", "zekey"),
            ("The Neighbors - Sweater Weather (slowed Reverb)", "sad"), // unpopular namesake uploader
            ("утекай", "тёмный принц"),
        ] {
            assert_eq!(
                placed(title, uploader, 100.0).await,
                Placement::Custom,
                "{title}"
            );
        }
    }

    #[test]
    fn apply_tags_each_placement() {
        let base = crate::testutil::m(
            json!({"title": "Клуб88", "artist": "uploader", "cover": "https://i.ytimg.com/vi/x/hq.jpg"}),
        );
        let mut meta = base.clone();
        apply(&mut meta, &Placement::Custom);
        assert_eq!(
            (
                &meta["artist"],
                &meta["album_artist"],
                &meta["album"],
                &meta["title"]
            ),
            (
                &json!("Custom"),
                &json!("Custom"),
                &json!("Custom"),
                &json!("Клуб88")
            )
        );
        let mut meta = base.clone();
        apply(
            &mut meta,
            &Placement::Unofficial {
                artists: unofficial(&["FORTUNA 812", "Юпи"]),
                picture: Some("https://img/1.jpg".into()),
            },
        );
        assert_eq!(meta["artists"], json!(["FORTUNA 812", "Юпи"]));
        assert_eq!(
            (
                &meta["album_artist"],
                &meta["album"],
                &meta["album_cover"],
                &meta["title"]
            ),
            (
                &json!("FORTUNA 812"),
                &json!("Unofficial"),
                &json!("https://img/1.jpg"),
                &json!("Клуб88")
            )
        );
        let mut meta = base;
        meta.insert("source".into(), json!("youtube"));
        let full = crate::testutil::m(
            json!({"title": "Клуб 88", "album": "A", "source": "deezer", "isrc": null}),
        );
        apply(&mut meta, &Placement::Official(full));
        assert_eq!(
            (&meta["title"], &meta["album"], &meta["source"]),
            (&json!("Клуб 88"), &json!("A"), &json!("youtube"))
        );
    }
}
