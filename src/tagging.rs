//! Canonical tags for imported files.
//!
//! Files arrive with all foreign metadata stripped (see `audio::Tools::normalize`), so
//! only the catalog's own values end up in the library. That matters for Navidrome's
//! album grouping: stray release dates, MusicBrainz ids or "comment=youtube" tags
//! split one album into several in the first proxy generation.
//!
//! `ARTIST` carries the display credit ("TVETH, Bato"), the multi-valued `ARTISTS`
//! tag carries each performer, so Navidrome lists the track under every artist of a
//! collaboration.
//!
//! All functions are blocking file I/O; async callers use `spawn_blocking`.

use crate::util::{Json, as_str, as_uint};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::id3::v2::{AttachedPictureFrame, ExtendedTextFrame, Frame, FrameId, Id3v2Tag, TextInformationFrame};
use lofty::mp4::{AdvisoryRating, Atom, AtomData, AtomIdent, DataType, Ilst, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{ItemKey, TagExt};
use lofty::TextEncoding;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TagError(pub String);

impl From<lofty::error::LoftyError> for TagError {
    fn from(e: lofty::error::LoftyError) -> Self {
        TagError(e.to_string())
    }
}

impl From<std::io::Error> for TagError {
    fn from(e: std::io::Error) -> Self {
        TagError(e.to_string())
    }
}

/// MIME of an embeddable cover. MP4 `covr` and most ID3 readers only know JPEG and
/// PNG, so anything else (WebP thumbnails) must be converted first, never mislabeled.
fn cover_mime(cover: &[u8]) -> Option<MimeType> {
    if cover.starts_with(b"\x89PNG") {
        Some(MimeType::Png)
    } else if cover.starts_with(b"\xff\xd8") {
        Some(MimeType::Jpeg)
    } else {
        None
    }
}

fn picture(cover: &[u8]) -> Option<Picture> {
    let Some(mime) = cover_mime(cover) else {
        tracing::warn!("not embedding cover: not JPEG/PNG ({} bytes)", cover.len());
        return None;
    };
    Some(Picture::new_unchecked(PictureType::CoverFront, Some(mime), Some("Cover".into()), cover.to_vec()))
}

fn suffix(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase()
}

fn itunes(name: &'static str) -> AtomIdent<'static> {
    AtomIdent::Freeform { mean: Cow::Borrowed("com.apple.iTunes"), name: Cow::Borrowed(name) }
}

fn text_frame(id: &'static str, value: String) -> Frame<'static> {
    Frame::Text(TextInformationFrame::new(FrameId::Valid(Cow::Borrowed(id)), TextEncoding::UTF8, value))
}

fn user_text(desc: &str, values: &[String]) -> Frame<'static> {
    // ID3v2.4 separates multiple values with NUL.
    Frame::UserText(ExtendedTextFrame::new(TextEncoding::UTF8, desc.into(), values.join("\0")))
}

fn picture_frame(pic: Picture) -> Frame<'static> {
    Frame::Picture(AttachedPictureFrame::new(TextEncoding::UTF8, pic))
}

/// Replace every tag of `path` (.m4a or .mp3) with the canonical ones from `meta`.
pub fn write_tags(path: &Path, meta: &Json, cover: Option<&[u8]>) -> Result<(), TagError> {
    let text = |k: &str| as_str(meta.get(k));
    let title = text("title");
    let mut artists: Vec<String> = meta
        .get("artists")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|v| as_str(Some(v))).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    if artists.is_empty() && !text("artist").is_empty() {
        artists.push(text("artist"));
    }
    let display = Some(text("artist")).filter(|s| !s.is_empty()).unwrap_or_else(|| artists.join(", "));
    let album = text("album");
    let album_artist = Some(text("album_artist")).filter(|s| !s.is_empty()).unwrap_or_else(|| artists.first().cloned().unwrap_or_default());
    let track = as_uint(meta.get("track")).min(u16::MAX as i64) as u16;
    let total = as_uint(meta.get("track_total")).min(u16::MAX as i64) as u16;
    let disc = match as_uint(meta.get("disc")).min(u16::MAX as i64) as u16 {
        0 => 1,
        d => d,
    };
    let year: String = text("year").chars().take(4).collect();
    let genre = text("genre");
    let isrc = text("isrc");
    let explicit = crate::util::truthy(meta.get("explicit"));
    let options = WriteOptions::default();

    match suffix(path).as_str() {
        "m4a" => {
            // A fresh ilst replaces every existing atom (ffmpeg's encoder tag too).
            let mut tags = Ilst::new();
            let utf8 = |tags: &mut Ilst, ident: AtomIdent<'static>, v: String| tags.insert(Atom::new(ident, AtomData::UTF8(v)));
            let pair = |a: u16, b: u16, pad: bool| {
                let mut v = vec![0, 0];
                v.extend(a.to_be_bytes());
                v.extend(b.to_be_bytes());
                if pad {
                    v.extend([0, 0]);
                }
                AtomData::Unknown { code: DataType::Reserved, data: v }
            };
            utf8(&mut tags, AtomIdent::Fourcc(*b"\xa9nam"), title);
            utf8(&mut tags, AtomIdent::Fourcc(*b"\xa9ART"), display);
            if let Some(atom) = Atom::from_collection(itunes("ARTISTS"), artists.into_iter().map(AtomData::UTF8).collect()) {
                tags.insert(atom);
            }
            if !album.is_empty() {
                utf8(&mut tags, AtomIdent::Fourcc(*b"\xa9alb"), album);
                utf8(&mut tags, AtomIdent::Fourcc(*b"aART"), album_artist);
            }
            if track > 0 {
                tags.insert(Atom::new(AtomIdent::Fourcc(*b"trkn"), pair(track, total, true)));
            }
            tags.insert(Atom::new(AtomIdent::Fourcc(*b"disk"), pair(disc, 0, false)));
            if !year.is_empty() {
                utf8(&mut tags, AtomIdent::Fourcc(*b"\xa9day"), year);
            }
            if !genre.is_empty() {
                utf8(&mut tags, AtomIdent::Fourcc(*b"\xa9gen"), genre);
            }
            if !isrc.is_empty() {
                utf8(&mut tags, itunes("ISRC"), isrc);
            }
            if explicit {
                tags.set_advisory_rating(AdvisoryRating::Explicit);
            }
            if let Some(pic) = cover.and_then(picture) {
                tags.insert_picture(pic);
            }
            tags.save_to_path(path, options)?;
            Ok(())
        }
        "mp3" => {
            let mut tags = Id3v2Tag::new();
            tags.insert(text_frame("TIT2", title));
            tags.insert(text_frame("TPE1", display));
            if !artists.is_empty() {
                tags.insert(user_text("ARTISTS", &artists));
            }
            if !album.is_empty() {
                tags.insert(text_frame("TALB", album));
                tags.insert(text_frame("TPE2", album_artist));
            }
            if track > 0 {
                tags.insert(text_frame("TRCK", if total > 0 { format!("{track}/{total}") } else { track.to_string() }));
            }
            tags.insert(text_frame("TPOS", disc.to_string()));
            if !year.is_empty() {
                tags.insert(text_frame("TDRC", year));
            }
            if !genre.is_empty() {
                tags.insert(text_frame("TCON", genre));
            }
            if !isrc.is_empty() {
                tags.insert(text_frame("TSRC", isrc));
            }
            if explicit {
                // iTunes' explicit flag for ID3, the MP3 counterpart of MP4 `rtng`.
                tags.insert(user_text("ITUNESADVISORY", &["1".into()]));
            }
            if let Some(pic) = cover.and_then(picture) {
                tags.insert(picture_frame(pic));
            }
            tags.save_to_path(path, options)?;
            Ok(())
        }
        other => Err(TagError(format!("cannot tag .{other} files"))),
    }
}

fn read_mp4(path: &Path) -> Result<Mp4File, TagError> {
    Ok(Mp4File::read_from(&mut File::open(path)?, ParseOptions::new().read_properties(false))?)
}

fn read_mp3(path: &Path) -> Result<MpegFile, TagError> {
    Ok(MpegFile::read_from(&mut File::open(path)?, ParseOptions::new().read_properties(false))?)
}

/// The first embedded picture, if any.
pub fn embedded_cover(path: &Path) -> Option<Vec<u8>> {
    let tagged = lofty::read_from_path(path).ok()?;
    tagged.tags().iter().find_map(|t| t.pictures().first().map(|p| p.data().to_vec()))
}

/// Replace only the embedded artwork, leaving every other tag as it is.
pub fn set_cover(path: &Path, cover: &[u8]) -> Result<(), TagError> {
    let pic = picture(cover).ok_or_else(|| TagError("cover is not JPEG or PNG".into()))?;
    match suffix(path).as_str() {
        "m4a" | "mp4" => {
            let mut tags = read_mp4(path)?.ilst().cloned().unwrap_or_default();
            tags.remove_pictures();
            tags.insert_picture(pic);
            tags.save_to_path(path, WriteOptions::default())?;
        }
        "mp3" => {
            let mut tags = read_mp3(path)?.id3v2().cloned().unwrap_or_default();
            tags.retain(|f| !matches!(f, Frame::Picture(_)));
            tags.insert(picture_frame(pic));
            tags.save_to_path(path, WriteOptions::default())?;
        }
        other => return Err(TagError(format!("cannot tag .{other} files"))),
    }
    Ok(())
}

/// Easy-mode tag view (mutagen's `easy=True` keys), used by tests and the migration tool.
pub fn read_tags(path: &Path) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let Ok(tagged) = lofty::read_from_path(path) else { return out };
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else { return out };
    let mut put = |name: &str, values: Vec<String>| {
        let values: Vec<String> = values.iter().flat_map(|v| v.split('\0')).filter(|v| !v.is_empty()).map(String::from).collect();
        if !values.is_empty() {
            out.insert(name.to_string(), values);
        }
    };
    let strings = |key: ItemKey| tag.get_strings(&key).map(String::from).collect::<Vec<_>>();
    let pair = |n: ItemKey, total: ItemKey| -> Vec<String> {
        let Some(n) = tag.get_string(&n) else { return vec![] };
        match tag.get_string(&total).filter(|t| *t != "0") {
            Some(t) => vec![format!("{n}/{t}")],
            None => vec![n.to_string()],
        }
    };
    put("title", strings(ItemKey::TrackTitle));
    put("artist", strings(ItemKey::TrackArtist));
    // lofty's generic view keeps only the first value of a multi-valued MP4 atom.
    let mp4_artists = matches!(suffix(path).as_str(), "m4a" | "mp4")
        .then(|| read_mp4(path).ok()?.ilst()?.get(&itunes("ARTISTS")).map(|a| a.data().filter_map(|d| if let AtomData::UTF8(s) = d { Some(s.clone()) } else { None }).collect()))
        .flatten();
    put("artists", mp4_artists.unwrap_or_else(|| strings(ItemKey::TrackArtists)));
    put("album", strings(ItemKey::AlbumTitle));
    put("albumartist", strings(ItemKey::AlbumArtist));
    put("tracknumber", pair(ItemKey::TrackNumber, ItemKey::TrackTotal));
    put("discnumber", pair(ItemKey::DiscNumber, ItemKey::DiscTotal));
    let date = strings(ItemKey::RecordingDate);
    put("date", if date.is_empty() { strings(ItemKey::Year) } else { date });
    put("genre", strings(ItemKey::Genre));
    put("isrc", strings(ItemKey::Isrc));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::{have, make, need};
    use serde_json::json;

    fn meta() -> Json {
        json!({
            "title": "Hella Quality", "artist": "TVETH, Bato", "artists": ["TVETH", "Bato"],
            "album": "RAW", "album_artist": "TVETH", "track": 3, "track_total": 12, "disc": 1,
            "year": 2026, "genre": "Rap", "isrc": "QZ1", "explicit": true, "duration": 98,
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    const JPEG: &[u8] = b"\xff\xd8\xff\xe0000000000000000000000000000000000000000000000";
    const WEBP: &[u8] = b"RIFF\x10\0\0\0WEBPVP8 0000000000000000";

    fn ilst(path: &Path) -> Ilst {
        read_mp4(path).unwrap().ilst().cloned().unwrap()
    }

    fn atom_text(tags: &Ilst, ident: AtomIdent<'_>) -> Vec<String> {
        tags.get(&ident)
            .map(|a| a.data().filter_map(|d| if let AtomData::UTF8(s) = d { Some(s.clone()) } else { None }).collect())
            .unwrap_or_default()
    }

    fn raw(tags: &Ilst, fourcc: &[u8; 4]) -> Vec<u8> {
        match tags.get(&AtomIdent::Fourcc(*fourcc)).and_then(|a| a.data().next().cloned()) {
            Some(AtomData::Unknown { data, .. }) => data,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn m4a_tags() {
        need!("ffmpeg");
        let dir = tempfile::tempdir().unwrap();
        let f = make(&dir.path().join("a.m4a"), 6, "aac", false);
        write_tags(&f, &meta(), Some(JPEG)).unwrap();
        let tags = ilst(&f);
        assert_eq!(atom_text(&tags, AtomIdent::Fourcc(*b"\xa9ART")), ["TVETH, Bato"]);
        assert_eq!(atom_text(&tags, itunes("ARTISTS")), ["TVETH", "Bato"]);
        assert_eq!(atom_text(&tags, AtomIdent::Fourcc(*b"aART")), ["TVETH"]);
        assert_eq!(atom_text(&tags, AtomIdent::Fourcc(*b"\xa9day")), ["2026"]);
        assert_eq!(atom_text(&tags, itunes("ISRC")), ["QZ1"]);
        assert_eq!(raw(&tags, b"trkn"), [0, 0, 0, 3, 0, 12, 0, 0]);
        assert_eq!(raw(&tags, b"disk"), [0, 0, 0, 1, 0, 0]);
        assert_eq!(tags.advisory_rating(), Some(AdvisoryRating::Explicit));
        assert_eq!(embedded_cover(&f).unwrap(), JPEG);
        let read = read_tags(&f);
        assert_eq!(read["tracknumber"], ["3/12"]);
        assert_eq!(read["artists"], ["TVETH", "Bato"]);
        assert_eq!(read["title"], ["Hella Quality"]);
    }

    #[test]
    fn retagging_replaces_everything() {
        need!("ffmpeg");
        let dir = tempfile::tempdir().unwrap();
        let f = make(&dir.path().join("a.m4a"), 6, "aac", false);
        write_tags(&f, &meta(), Some(JPEG)).unwrap();
        let only = json!({"title": "Only Title", "artist": "X"}).as_object().cloned().unwrap();
        write_tags(&f, &only, None).unwrap();
        let tags = ilst(&f);
        assert_eq!(atom_text(&tags, AtomIdent::Fourcc(*b"\xa9nam")), ["Only Title"]);
        assert!(tags.get(&AtomIdent::Fourcc(*b"\xa9alb")).is_none());
        assert!(tags.get(&AtomIdent::Fourcc(*b"\xa9gen")).is_none());
        assert!(tags.get(&AtomIdent::Fourcc(*b"trkn")).is_none());
        assert!(tags.advisory_rating().is_none());
        assert!(embedded_cover(&f).is_none());
        assert_eq!(atom_text(&tags, itunes("ARTISTS")), ["X"], "artists fall back to the display artist");
    }

    #[test]
    fn mp3_tags() {
        need!("ffmpeg");
        let dir = tempfile::tempdir().unwrap();
        let f = make(&dir.path().join("a.mp3"), 6, "mp3", false);
        write_tags(&f, &meta(), Some(JPEG)).unwrap();
        let read = read_tags(&f);
        assert_eq!(read["title"], ["Hella Quality"]);
        assert_eq!(read["artist"], ["TVETH, Bato"]);
        assert_eq!(read["albumartist"], ["TVETH"]);
        assert_eq!(read["tracknumber"], ["3/12"]);
        assert_eq!(read["discnumber"], ["1"]);
        assert_eq!(read["date"], ["2026"]);
        assert_eq!(read["isrc"], ["QZ1"]);
        assert_eq!(read["artists"], ["TVETH", "Bato"]);
        let id3 = read_mp3(&f).unwrap().id3v2().cloned().unwrap();
        assert_eq!(id3.get_user_text("ITUNESADVISORY"), Some("1"), "explicit kept on MP3 (B10)");
        assert_eq!(embedded_cover(&f).unwrap(), JPEG);
    }

    #[test]
    fn set_cover_replaces_only_artwork() {
        need!("ffmpeg");
        let dir = tempfile::tempdir().unwrap();
        let png: &[u8] = b"\x89PNG\r\n\x1a\n0000000000000000";
        for (name, codec) in [("a.m4a", "aac"), ("a.mp3", "mp3")] {
            let f = make(&dir.path().join(name), 6, codec, false);
            write_tags(&f, &meta(), Some(JPEG)).unwrap();
            set_cover(&f, png).unwrap();
            assert_eq!(embedded_cover(&f).unwrap(), png, "{name}");
            assert_eq!(read_tags(&f)["title"], ["Hella Quality"], "{name}");
            assert!(set_cover(&f, WEBP).is_err());
        }
    }

    #[test]
    fn webp_cover_is_never_embedded_mislabeled() {
        need!("ffmpeg");
        let dir = tempfile::tempdir().unwrap();
        let f = make(&dir.path().join("a.m4a"), 6, "aac", false);
        write_tags(&f, &meta(), Some(WEBP)).unwrap();
        assert!(embedded_cover(&f).is_none());
        assert_eq!(read_tags(&f)["title"], ["Hella Quality"]);
    }

    #[test]
    fn unsupported_suffix() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_tags(&dir.path().join("x.flac"), &meta(), None).is_err());
        assert!(set_cover(&dir.path().join("x.flac"), JPEG).is_err());
        assert!(read_tags(&dir.path().join("missing.m4a")).is_empty());
        assert!(embedded_cover(&dir.path().join("missing.m4a")).is_none());
    }
}
