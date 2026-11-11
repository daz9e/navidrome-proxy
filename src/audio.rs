//! Audio container handling with ffmpeg/ffprobe, and Chromaprint fingerprints.
//!
//! YouTube delivers AAC as a *fragmented DASH* MP4 (`major_brand=dash`). Without a
//! remux, AVFoundation (Arpeggi's AVQueuePlayer) reads such a file with a wrong,
//! roughly doubled duration: the progress bar runs past the real end into negative
//! time and the queue never advances to the next song. Every downloaded file is
//! therefore rewritten into a plain MP4 with the index at the front (`faststart`),
//! which also makes the first byte playable while the rest is still transferring.

use crate::process::{self, RunError, args};
use crate::util::{Json, as_f64, as_str, as_uint};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const AUDIO_SUFFIXES: [&str; 11] = [
    "m4a", "mp4", "mp3", "opus", "ogg", "oga", "webm", "flac", "wav", "aac", "mka",
];

/// Shorter than this is a preview or a broken file, not a song.
pub const MIN_SECONDS: f64 = 5.0;

#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct AudioError(pub String);

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AudioInfo {
    pub duration: f64,
    pub codec: String,
    /// kbit/s
    pub bitrate: i64,
    pub sample_rate: i64,
    pub channels: i64,
    pub size: u64,
    pub suffix: String,
    pub brand: String,
}

impl AudioInfo {
    pub fn as_json(&self) -> Json {
        let v = json!({
            "duration": self.duration, "codec": self.codec, "bitrate": self.bitrate,
            "sample_rate": self.sample_rate, "channels": self.channels, "size": self.size,
            "suffix": self.suffix, "brand": self.brand,
        });
        v.as_object().cloned().unwrap_or_default()
    }
}

/// Paths of the external tools.
#[derive(Debug, Clone)]
pub struct Tools {
    pub ffmpeg: String,
    pub ffprobe: String,
    pub fpcalc: String,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            fpcalc: "fpcalc".into(),
        }
    }
}

pub fn suffix_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// Container the library stores a codec in; everything exotic becomes AAC/m4a.
pub fn target_suffix(codec: &str) -> &'static str {
    if codec == "mp3" { "mp3" } else { "m4a" }
}

fn tail_text(bytes: &[u8]) -> String {
    crate::util::tail(String::from_utf8_lossy(bytes).trim(), 300)
}

fn run_error(e: RunError) -> AudioError {
    AudioError(e.to_string())
}

impl Tools {
    pub async fn probe(&self, path: &Path) -> Result<AudioInfo, AudioError> {
        let out = process::run(
            &self.ffprobe,
            &args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
                "-select_streams",
                "a:0",
            ])
            .into_iter()
            .chain([path.display().to_string()])
            .collect::<Vec<_>>(),
            Duration::from_secs(60),
            None,
        )
        .await
        .map_err(run_error)?;
        if !out.success() {
            return Err(AudioError(format!(
                "ffprobe failed: {}",
                tail_text(&out.stderr)
            )));
        }
        let data: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        let stream = data
            .get("streams")
            .and_then(Value::as_array)
            .and_then(|s| s.first())
            .cloned()
            .ok_or_else(|| AudioError("no audio stream".into()))?;
        let fmt = data.get("format").cloned().unwrap_or(Value::Null);
        let pick = |k: &str| {
            stream
                .get(k)
                .filter(|v| crate::util::truthy(Some(v)))
                .or_else(|| fmt.get(k))
        };
        Ok(AudioInfo {
            duration: as_f64(pick("duration")).unwrap_or(0.0),
            codec: as_str(stream.get("codec_name")),
            bitrate: as_uint(pick("bit_rate")) / 1000,
            sample_rate: as_uint(stream.get("sample_rate")),
            channels: as_uint(stream.get("channels")),
            size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            suffix: suffix_of(path),
            brand: as_str(fmt.get("tags").and_then(|t| t.get("major_brand"))),
        })
    }

    /// Produce a clean, player-friendly file from any downloaded source.
    ///
    /// * AAC  -> stream copy into a plain MP4 with `+faststart` (fixes DASH files)
    /// * MP3  -> stream copy, all foreign tags dropped
    /// * else -> transcode to AAC 192 kbit/s in MP4 (Opus/Vorbis/WebM are not
    ///   reliably playable by AVFoundation over HTTP)
    ///
    /// Metadata is always dropped; canonical tags are written afterwards.
    pub async fn normalize(
        &self,
        src: &Path,
        dest_dir: &Path,
        stem: &str,
    ) -> Result<(PathBuf, AudioInfo), AudioError> {
        let info = self.probe(src).await?;
        if info.duration < MIN_SECONDS {
            return Err(AudioError(format!(
                "audio too short ({:.1}s)",
                info.duration
            )));
        }
        let dest = dest_dir.join(format!("{stem}.{}", target_suffix(&info.codec)));
        let mut a = args(["-nostdin", "-v", "error", "-y", "-i"]);
        a.push(src.display().to_string());
        a.extend(args([
            "-map",
            "0:a:0",
            "-map_metadata",
            "-1",
            "-vn",
            "-sn",
            "-dn",
        ]));
        a.extend(args(match info.codec.as_str() {
            "aac" => vec!["-c:a", "copy", "-movflags", "+faststart", "-f", "mp4"],
            "mp3" => vec!["-c:a", "copy", "-id3v2_version", "3", "-f", "mp3"],
            _ => vec![
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
            ],
        }));
        a.push(dest.display().to_string());
        let out = process::run(&self.ffmpeg, &a, Duration::from_secs(600), None)
            .await
            .map_err(run_error)?;
        if !out.success() || !dest.is_file() {
            return Err(AudioError(format!(
                "ffmpeg failed: {}",
                tail_text(&out.stderr)
            )));
        }
        let result = self.probe(&dest).await?;
        if info.duration > 0.0 && (result.duration - info.duration).abs() > 2.0 {
            return Err(AudioError(format!(
                "duration changed while remuxing: {:.1}s -> {:.1}s",
                info.duration, result.duration
            )));
        }
        Ok((dest, result))
    }

    /// Rewrite a DASH-fragmented MP4 in place (lossless). Returns true if changed.
    ///
    /// Existing tags are preserved (`-map_metadata 0`), unlike `normalize`.
    pub async fn remux_in_place(&self, path: &Path) -> Result<bool, AudioError> {
        let info = self.probe(path).await?;
        if info.codec != "aac" || info.brand != "dash" {
            return Ok(false);
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("audio");
        let tmp = path.with_file_name(format!(".{stem}.remux.{}", suffix_of(path)));
        let mut a = args(["-nostdin", "-v", "error", "-y", "-i"]);
        a.push(path.display().to_string());
        a.extend(args([
            "-map",
            "0",
            "-map_metadata",
            "0",
            "-c",
            "copy",
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
        ]));
        a.push(tmp.display().to_string());
        let out = process::run(&self.ffmpeg, &a, Duration::from_secs(600), None)
            .await
            .map_err(run_error)?;
        if !out.success() || !tmp.is_file() {
            let _ = std::fs::remove_file(&tmp);
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            return Err(AudioError(format!(
                "remux failed for {name}: {}",
                tail_text(&out.stderr)
            )));
        }
        let fixed = self.probe(&tmp).await?;
        if (fixed.duration - info.duration).abs() > 2.0 && fixed.duration > 0.0 {
            // The DASH header lies about duration; the rewritten file is the truth.
            tracing::info!(
                "remux {}: header duration {:.1}s -> real {:.1}s",
                path.display(),
                info.duration,
                fixed.duration
            );
        }
        std::fs::rename(&tmp, path).map_err(|e| AudioError(e.to_string()))?;
        Ok(true)
    }

    /// Convert any image ffmpeg reads (WebP thumbnails) to JPEG for embedding.
    ///
    /// `square` crops the centre square; a 4:3 frame is a 16:9 picture between black
    /// bars (YouTube's `hqdefault`), so its square is the picture's height.
    pub async fn to_jpeg(&self, image: &[u8], work_dir: &Path, square: bool) -> Option<Vec<u8>> {
        let unique = rand::random::<u64>();
        let src = work_dir.join(format!("cover-{unique:x}.src"));
        let dest = work_dir.join(format!("cover-{unique:x}.jpg"));
        std::fs::create_dir_all(work_dir).ok()?;
        std::fs::write(&src, image).ok()?;
        let mut a = args(["-nostdin", "-v", "error", "-y", "-i"]);
        a.push(src.display().to_string());
        if square {
            let side = "if(eq(iw*3,ih*4),iw*9/16,min(iw,ih))";
            a.extend(args(["-vf", &format!("crop=w='{side}':h='{side}'")]));
        }
        a.extend(args(["-frames:v", "1", "-q:v", "2"]));
        a.push(dest.display().to_string());
        let out = process::run(&self.ffmpeg, &a, Duration::from_secs(60), None)
            .await
            .ok()?;
        let data = out.success().then(|| std::fs::read(&dest).ok()).flatten();
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dest);
        data
    }

    /// Raw Chromaprint fingerprint (needs `fpcalc`), or None if unavailable.
    pub async fn fingerprint(&self, path: &Path, length: u32) -> Option<Vec<u32>> {
        let mut a = args(["-raw", "-json", "-length"]);
        a.push(length.to_string());
        a.push(path.display().to_string());
        let out = process::run(&self.fpcalc, &a, Duration::from_secs(120), None)
            .await
            .ok()?;
        if !out.success() {
            return None;
        }
        let v: Value = serde_json::from_slice(&out.stdout).ok()?;
        v.get("fingerprint")?
            .as_array()?
            .iter()
            .map(|x| x.as_i64().map(|n| n as u32))
            .collect()
    }

    pub async fn available(&self) -> Vec<(&'static str, bool)> {
        let mut out = Vec::new();
        for (name, tool) in [
            ("ffmpeg", &self.ffmpeg),
            ("ffprobe", &self.ffprobe),
            ("fpcalc", &self.fpcalc),
        ] {
            let ok = process::run(tool, &args(["-version"]), Duration::from_secs(10), None)
                .await
                .is_ok();
            out.push((name, ok));
        }
        out
    }
}

/// Best bitwise similarity (0..1) of two raw fingerprints over small time offsets.
pub fn fingerprint_similarity(a: &[u32], b: &[u32], max_offset: i64) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut best: f64 = 0.0;
    for offset in -max_offset..=max_offset {
        let (mut diff, mut pairs) = (0u64, 0u64);
        for (i, x) in a.iter().enumerate() {
            let j = i as i64 + offset;
            if j >= 0 && (j as usize) < b.len() {
                diff += (x ^ b[j as usize]).count_ones() as u64;
                pairs += 1;
            }
        }
        if pairs >= 30 {
            best = best.max(1.0 - diff as f64 / (32 * pairs) as f64);
        }
    }
    best
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Whether the real tools are on PATH (inside `nix develop` / the Nix build they are).
    pub fn have(tool: &str) -> bool {
        std::process::Command::new(tool)
            .arg("-version")
            .output()
            .is_ok()
    }

    /// A synthetic tone file made with ffmpeg (no copyrighted material in the repo).
    pub fn make(path: &Path, seconds: u32, codec: &str, dash: bool) -> PathBuf {
        let mut a: Vec<String> = args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"]);
        a.push(format!("sine=frequency=440:duration={seconds}"));
        match codec {
            "aac" => {
                a.extend(args(["-c:a", "aac", "-b:a", "128k"]));
                if dash {
                    a.extend(args([
                        "-movflags",
                        "frag_keyframe+empty_moov+default_base_moof",
                        "-brand",
                        "dash",
                        "-f",
                        "mp4",
                    ]));
                }
            }
            "mp3" => a.extend(args([
                "-c:a",
                "libmp3lame",
                "-b:a",
                "192k",
                "-metadata",
                "title=junk",
                "-metadata",
                "comment=youtube",
            ])),
            "opus" => a.extend(args(["-c:a", "libopus", "-b:a", "96k", "-f", "webm"])),
            _ => unreachable!(),
        }
        a.push(path.display().to_string());
        assert!(
            std::process::Command::new("ffmpeg")
                .args(&a)
                .status()
                .unwrap()
                .success()
        );
        path.to_path_buf()
    }

    /// A melody-like signal with changing notes, so fingerprints carry information.
    pub fn make_music(path: &Path, seconds: u32, variant: u32) -> PathBuf {
        let notes: [u32; 8] = if variant == 0 {
            [220, 262, 294, 330, 349, 392, 440, 494]
        } else {
            [247, 311, 370, 415, 466, 523, 587, 622]
        };
        let expr: Vec<String> = notes
            .iter()
            .enumerate()
            .map(|(i, f)| format!("0.3*sin(2*PI*{f}*t)*between(mod(t\\,8)\\,{i}\\,{})", i + 1))
            .collect();
        let status = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("aevalsrc={}:s=44100:d={seconds}", expr.join("+")))
            .args(["-c:a", "aac", "-b:a", "128k"])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
        path.to_path_buf()
    }

    macro_rules! need {
        ($($tool:expr),*) => {
            $(if !have($tool) { eprintln!("skipped: {} missing", $tool); return; })*
        };
    }
    pub(crate) use need;

    #[tokio::test]
    async fn covers_are_cropped_to_the_centre_square() {
        need!("ffmpeg", "ffprobe");
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::default();
        // hqdefault (4:3 with bars), maxresdefault (16:9), already square.
        for ((w, h), side) in [((480, 360), 270), ((1280, 720), 720), ((500, 500), 500)] {
            let src = dir.path().join("src.jpg");
            let made = std::process::Command::new("ffmpeg")
                .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"])
                .arg(format!("color=red:s={w}x{h}"))
                .args(["-frames:v", "1"])
                .arg(&src)
                .status()
                .unwrap();
            assert!(made.success());
            let image = std::fs::read(&src).unwrap();
            let out = tools.to_jpeg(&image, dir.path(), true).await.unwrap();
            let probe = dir.path().join("out.jpg");
            std::fs::write(&probe, &out).unwrap();
            let dims = std::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "stream=width,height",
                    "-of",
                    "csv=p=0",
                ])
                .arg(&probe)
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&dims.stdout).trim(),
                format!("{side},{side}"),
                "{w}x{h}"
            );
        }
    }

    #[tokio::test]
    async fn dash_aac_is_remuxed_to_plain_faststart_mp4() {
        need!("ffmpeg");
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::default();
        let src = make(&dir.path().join("source.m4a"), 30, "aac", true);
        assert_eq!(tools.probe(&src).await.unwrap().brand, "dash");
        let (out, info) = tools.normalize(&src, dir.path(), "audio").await.unwrap();
        assert_eq!(suffix_of(&out), "m4a");
        assert!(info.codec == "aac" && info.brand != "dash");
        assert!((info.duration - 30.0).abs() < 0.5);
        let data = std::fs::read(&out).unwrap();
        let find = |n: &[u8]| data.windows(4).position(|w| w == n).unwrap();
        assert!(
            find(b"moov") < find(b"mdat"),
            "index before payload (faststart)"
        );
    }

    #[tokio::test]
    async fn opus_is_transcoded_and_mp3_copied_without_tags() {
        need!("ffmpeg");
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::default();
        let opus = make(&dir.path().join("source.webm"), 25, "opus", false);
        let (out, info) = tools
            .normalize(&opus, &dir.path().join("."), "a")
            .await
            .unwrap();
        assert!(suffix_of(&out) == "m4a" && info.codec == "aac");
        let mp3 = make(&dir.path().join("source.mp3"), 25, "mp3", false);
        let (out, info) = tools.normalize(&mp3, dir.path(), "b").await.unwrap();
        assert!(suffix_of(&out) == "mp3" && info.codec == "mp3");
        assert!(!crate::tagging::read_tags(&out).contains_key("title"));
    }

    #[tokio::test]
    async fn too_short_audio_is_rejected_and_remux_only_touches_dash() {
        need!("ffmpeg");
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::default();
        let short = make(&dir.path().join("short.m4a"), 3, "aac", false);
        assert!(tools.normalize(&short, dir.path(), "x").await.is_err());
        let dash = make(&dir.path().join("a.m4a"), 30, "aac", true);
        let plain = make(&dir.path().join("b.m4a"), 30, "aac", false);
        assert!(tools.remux_in_place(&dash).await.unwrap());
        assert_ne!(tools.probe(&dash).await.unwrap().brand, "dash");
        assert!(!tools.remux_in_place(&plain).await.unwrap());
    }

    #[tokio::test]
    async fn fingerprint_distinguishes_recordings() {
        need!("ffmpeg", "fpcalc");
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::default();
        let a = make_music(&dir.path().join("a.m4a"), 40, 0);
        let a2 = dir.path().join("a2.m4a");
        std::fs::copy(&a, &a2).unwrap();
        let b = make_music(&dir.path().join("b.m4a"), 40, 1);
        let (fa, fa2, fb) = (
            tools.fingerprint(&a, 120).await.unwrap(),
            tools.fingerprint(&a2, 120).await.unwrap(),
            tools.fingerprint(&b, 120).await.unwrap(),
        );
        assert!(fingerprint_similarity(&fa, &fa2, 80) > 0.95);
        assert!(fingerprint_similarity(&fa, &fb, 80) < fingerprint_similarity(&fa, &fa2, 80));
    }

    #[test]
    fn similarity_math() {
        let a: Vec<u32> = (0..100).map(|i| i * 7919).collect();
        assert_eq!(fingerprint_similarity(&a, &a, 5), 1.0);
        let shifted: Vec<u32> = std::iter::repeat_n(0, 3).chain(a.iter().copied()).collect();
        assert_eq!(fingerprint_similarity(&a, &shifted, 5), 1.0);
        assert_eq!(fingerprint_similarity(&a, &[], 5), 0.0);
        assert_eq!(
            fingerprint_similarity(&a[..10], &a[..10], 5),
            0.0,
            "too few pairs"
        );
    }

    #[tokio::test]
    async fn missing_tools_are_errors_not_panics() {
        let tools = Tools {
            ffmpeg: "/nope/ffmpeg".into(),
            ffprobe: "/nope/ffprobe".into(),
            fpcalc: "/nope/fpcalc".into(),
        };
        assert!(tools.probe(Path::new("/tmp/x")).await.is_err());
        assert!(tools.fingerprint(Path::new("/tmp/x"), 10).await.is_none());
        assert!(tools.available().await.iter().all(|(_, ok)| !ok));
    }
}
