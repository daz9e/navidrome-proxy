//! Text normalization and candidate scoring shared by search, sources and imports.

use crate::util::{Json, as_f64, as_str};
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

/// Words that mark a *different recording* than the original. A candidate carrying
/// one that the request does not carry is rejected for downloads (the "minus instead
/// of the original" incident with FORTUNA 812 — Black Water) and penalized in imports.
pub const VERSION_MARKERS: &[(&str, &[&str])] = &[
    (
        "instrumental",
        &[
            "instrumental",
            "инструментал",
            "минус",
            "minus",
            "backing track",
            "type beat",
            "beat",
            "no vocals",
            "без слов",
        ],
    ),
    ("karaoke", &["karaoke", "караоке"]),
    (
        "remix",
        &[
            "remix",
            "ремикс",
            "rmx",
            "flip",
            "bootleg",
            "mashup",
            "мэшап",
            "vip mix",
        ],
    ),
    ("live", &["live", "лайв", "концерт", "concert", "unplugged"]),
    (
        "speed",
        &[
            "sped up",
            "speed up",
            "speedup",
            "nightcore",
            "ускоренн",
            "slowed",
            "замедлен",
            "reverb",
            "daycore",
            "8d",
            "bass boosted",
        ],
    ),
    (
        "cover",
        &[
            "cover",
            "кавер",
            "tribute",
            "в стиле",
            "originally performed",
            "piano version",
        ],
    ),
    ("acoustic", &["acoustic", "акустик"]),
    ("tiktok", &["tiktok", "tik tok"]),
    ("extended", &["extended", "club mix", "radio edit", "edit"]),
    (
        "demo",
        &["demo", "демо", "snippet", "сниппет", "preview", "leak"],
    ),
    (
        "loop",
        &[
            "1 hour",
            "one hour",
            "10 hours",
            "hour loop",
            "1 час",
            "10 часов",
            "час версия",
            "looped",
            "best part",
        ],
    ),
];

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

/// Noise that video titles add around the real title.
static VIDEO_NOISE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)\b(official( music)?( video| audio| lyric video| visualizer)?|music video|lyric video|lyrics?|audio|video|clip|клип|премьера|premiere|hd|hq|4k|mv|visualizer|текст песни|полная версия|full version|prod\.? by [^()\[\]]*)\b",
    )
});
static FEAT: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)[\(\[]?\s*\b(feat\.?|ft\.?|featuring|при уч\.?|при участии)\s+[^()\[\]]*[\)\]]?")
});
static ARTIST_SPLIT: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\s*(?:,|;|&|/|\+|\bfeat\.?|\bft\.?|\bfeaturing|\bx\b|\bх\b|\band\b|\bи\b|\bwith\b)\s*")
});
static BRACKETS: LazyLock<Regex> = LazyLock::new(|| re(r"[\(\[\{][^\)\]\}]*[\)\]\}]"));
static NON_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"[^\w\s]"));
static NUMBER: LazyLock<Regex> = LazyLock::new(|| re(r"\d+"));
static TOPIC_SUFFIX: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\s*-\s*topic$"));
static EDGE_DASHES: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*[-–—:|]+|[-–—:|]+\s*$"));

/// Python's `str.casefold()` for the cases that matter here (`to_lowercase` plus the
/// full folds that change length).
fn casefold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            'ß' | 'ẞ' => out.push_str("ss"),
            'ς' => out.push('σ'),
            'ﬁ' => out.push_str("fi"),
            'ﬂ' => out.push_str("fl"),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// Case/diacritics/punctuation-insensitive form used for comparisons.
pub fn normalize(text: &str) -> String {
    let value: String = text.nfkc().collect();
    let value = casefold(&value).replace('ё', "е").replace(['’', '`'], "'");
    let value: String = value
        .nfkd()
        .filter(|c| canonical_combining_class(*c) == 0)
        .collect();
    let value = NON_WORD.replace_all(&value, " ");
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn translit(c: char) -> Option<&'static str> {
    Some(match c {
        'а' => "a",
        'б' => "b",
        'в' => "v",
        'г' => "g",
        'д' => "d",
        'е' => "e",
        'ж' => "zh",
        'з' => "z",
        'и' => "i",
        'й' => "y",
        'к' => "k",
        'л' => "l",
        'м' => "m",
        'н' => "n",
        'о' => "o",
        'п' => "p",
        'р' => "r",
        'с' => "s",
        'т' => "t",
        'у' => "u",
        'ф' => "f",
        'х' => "h",
        'ц' => "ts",
        'ч' => "ch",
        'ш' => "sh",
        'щ' => "sch",
        'ъ' => "",
        'ы' => "y",
        'ь' => "",
        'э' => "e",
        'ю' => "yu",
        'я' => "ya",
        _ => return None,
    })
}

/// `normalize` with Cyrillic transliterated, so "Братья Грим" meets "Bratya Grim".
pub fn latin(text: &str) -> String {
    let mut out = String::new();
    for c in normalize(text).chars() {
        match translit(c) {
            Some(s) => out.push_str(s),
            None => out.push(c),
        }
    }
    out
}

pub fn words(text: &str) -> HashSet<String> {
    normalize(text)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

pub fn split_artists(credit: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for part in ARTIST_SPLIT
        .split(credit)
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let key = normalize(part);
        if !key.is_empty() && seen.insert(key) {
            out.push(part.to_string());
        }
    }
    out
}

/// Title without featuring credits and bracketed annotations.
pub fn base_title(title: &str) -> String {
    let value = FEAT.replace_all(title, " ");
    let stripped = normalize(&BRACKETS.replace_all(&value, " "));
    if stripped.is_empty() {
        normalize(&value)
    } else {
        stripped
    }
}

pub fn markers(text: &str) -> BTreeSet<&'static str> {
    let norm = format!(" {} ", normalize(text));
    let mut found = BTreeSet::new();
    for (name, variants) in VERSION_MARKERS {
        for variant in *variants {
            let v = normalize(variant);
            if !v.is_empty()
                && (norm.contains(&format!(" {v} "))
                    || (v.chars().count() >= 6 && norm.contains(&v)))
            {
                found.insert(*name);
                break;
            }
        }
    }
    found
}

fn latin_words(text: &str) -> Vec<String> {
    latin(text).split_whitespace().map(String::from).collect()
}

/// Word position where `name` stands in `text`, whatever the punctuation, alphabet or
/// spacing around it ("FORTUNA 812" in "фортуна⋆812 - Рассвет" and "fortuna812-клуб88"),
/// allowing a typo in longer words ("Childish Gabino").
pub fn find_name(text: &str, name: &str) -> Option<usize> {
    let (hay, needle) = (latin_words(text), latin_words(name));
    if needle.is_empty() {
        return None;
    }
    let glued = needle.concat();
    let same = |h: &String, n: &String| {
        h == n || (h.chars().count() >= 5 && n.chars().count() >= 5 && ratio(h, n) >= 0.9)
    };
    (0..hay.len()).find(|&i| {
        (needle.len() > 1 && hay[i] == glued)
            || hay
                .get(i..i + needle.len())
                .is_some_and(|w| w.iter().zip(&needle).all(|(h, n)| same(h, n)))
    })
}

/// Normalized words of a video title worth a catalog search: no channel noise, no
/// version words ("slowed", "reverb"), and without bracketed parts unless `brackets`.
pub fn search_text(title: &str, brackets: bool) -> String {
    static VERSION_WORDS: LazyLock<HashSet<String>> = LazyLock::new(|| {
        VERSION_MARKERS
            .iter()
            .flat_map(|(_, variants)| variants.iter())
            .flat_map(|v| words(v))
            .filter(|w| w.chars().count() >= 3)
            .collect()
    });
    let mut text = VIDEO_NOISE.replace_all(title, " ").into_owned();
    if !brackets {
        text = BRACKETS.replace_all(&text, " ").into_owned();
    }
    normalize(&text)
        .split(' ')
        .filter(|w| !w.is_empty() && !VERSION_WORDS.contains(*w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parts of a title worth looking up as names: first split at punctuation next to a
/// space or brackets ("Elox1m & FORTUNA 812 -- Порезы (slowed)", keeping "фортуна⋆812"
/// whole), then at any punctuation ("фортуна812-клуб88").
pub fn segments(title: &str) -> Vec<String> {
    static SPACED: LazyLock<Regex> =
        LazyLock::new(|| re(r"\s+[^\p{L}\p{N}\s']+\s*|\s*[^\p{L}\p{N}\s']+\s+|[()\[\]{}]"));
    static ANY: LazyLock<Regex> = LazyLock::new(|| re(r"[^\p{L}\p{N}\s']+"));
    let mut seen = HashSet::new();
    SPACED
        .split(title)
        .chain(ANY.split(title))
        .map(str::trim)
        .filter(|s| !search_text(s, true).is_empty() && seen.insert(normalize(s)))
        .map(String::from)
        .collect()
}

/// `difflib.SequenceMatcher(None, a, b).ratio()`, including its autojunk heuristic.
pub fn ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0;
    }
    let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
    for (j, c) in b.iter().enumerate() {
        b2j.entry(*c).or_default().push(j);
    }
    if b.len() >= 200 {
        let ntest = b.len() / 100 + 1;
        b2j.retain(|_, idx| idx.len() <= ntest);
    }
    let longest = |alo: usize, ahi: usize, blo: usize, bhi: usize| -> (usize, usize, usize) {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for (i, ch) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut new: HashMap<usize, usize> = HashMap::new();
            if let Some(js) = b2j.get(ch) {
                for &j in js {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = j
                        .checked_sub(1)
                        .and_then(|p| j2len.get(&p))
                        .copied()
                        .unwrap_or(0)
                        + 1;
                    new.insert(j, k);
                    if k > bestsize {
                        (besti, bestj, bestsize) = (i + 1 - k, j + 1 - k, k);
                    }
                }
            }
            j2len = new;
        }
        // difflib extends the match over popular (autojunked) elements as well.
        while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && a[besti + bestsize] == b[bestj + bestsize]
        {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    };
    let mut matches = 0;
    let mut queue = vec![(0, a.len(), 0, b.len())];
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = longest(alo, ahi, blo, bhi);
        if k > 0 {
            matches += k;
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    2.0 * matches as f64 / total as f64
}

pub fn similarity(a: &str, b: &str) -> f64 {
    let (na, nb) = (normalize(a), normalize(b));
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    if na == nb {
        return 1.0;
    }
    ratio(&na, &nb)
}

/// Candidate credits: a list of names, or one credit string that is split too.
pub enum Credits<'a> {
    List(&'a [String]),
    Text(&'a str),
}

/// Share of requested artists found among the candidate's credits (0..1).
pub fn artists_match(wanted: &[String], candidate: Credits) -> f64 {
    let wanted_keys: Vec<String> = wanted
        .iter()
        .map(|a| normalize(a))
        .filter(|k| !k.is_empty())
        .collect();
    if wanted_keys.is_empty() {
        return 1.0;
    }
    let list: Vec<String> = match candidate {
        Credits::List(l) => l.to_vec(),
        Credits::Text(t) => {
            let mut l = split_artists(t);
            l.push(t.to_string());
            l
        }
    };
    let have: Vec<String> = list
        .iter()
        .map(|c| normalize(c))
        .filter(|k| !k.is_empty())
        .collect();
    let joined = format!(" {} ", have.join(" "));
    let have_latin: Vec<String> = have.iter().map(|h| latin(h)).collect();
    // "M.C." normalizes to "m c": compare without spaces as well.
    let squash = |s: &str| s.replace(' ', "");
    let mut hits = 0;
    for key in &wanted_keys {
        let same = have.contains(key)
            || joined.contains(&format!(" {key} "))
            || have
                .iter()
                .any(|h| similarity(key, h) >= 0.88 || (key.len() > 1 && squash(key) == squash(h)));
        if same
            || have_latin
                .iter()
                .any(|h| similarity(&latin(key), h) >= 0.88)
        {
            hits += 1;
        }
    }
    hits as f64 / wanted_keys.len() as f64
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wanted {
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration: Option<f64>,
    pub isrc: Option<String>,
}

impl Wanted {
    pub fn new(title: &str, artists: &[&str]) -> Self {
        Self {
            title: title.into(),
            artists: artists.iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        }
    }

    pub fn markers(&self) -> BTreeSet<&'static str> {
        let mut m = markers(&self.title);
        m.extend(markers(self.album.as_deref().unwrap_or("")));
        m
    }

    fn duration(&self) -> Option<f64> {
        self.duration.filter(|d| *d != 0.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scored {
    pub score: f64,
    pub reasons: Vec<String>,
    pub rejected: Option<&'static str>,
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn reject(reason: String, why: &'static str) -> Scored {
    Scored {
        score: 0.0,
        reasons: vec![reason],
        rejected: Some(why),
    }
}

/// Artists of a track meta dict: `artists` list, else the `artist` credit.
pub fn meta_artists(cand: &Json) -> Vec<String> {
    match cand.get("artists") {
        Some(Value::Array(a)) if !a.is_empty() => a.iter().map(|v| as_str(Some(v))).collect(),
        _ => {
            let artist = as_str(cand.get("artist"));
            let mut l = split_artists(&artist);
            if !artist.is_empty() {
                l.push(artist);
            }
            l
        }
    }
}

/// Score a Deezer candidate for a free-text `artist - title` request.
/// `cand` is proxy track metadata (see `deezer::track_meta`).
pub fn score_catalog_candidate(wanted: &Wanted, cand: &Json) -> Scored {
    let mut reasons = Vec::new();
    let cand_isrc = as_str(cand.get("isrc"));
    if let Some(isrc) = wanted.isrc.as_deref().filter(|i| !i.is_empty())
        && !cand_isrc.is_empty()
        && isrc.to_uppercase() == cand_isrc.to_uppercase()
    {
        return Scored {
            score: 100.0,
            reasons: vec!["isrc".into()],
            rejected: None,
        };
    }
    let title = as_str(cand.get("title"));
    let full_title = format!("{} {}", title, as_str(cand.get("title_version")))
        .trim()
        .to_string();
    let mut score = 0.0;
    if normalize(&full_title) == normalize(&wanted.title)
        || normalize(&title) == normalize(&wanted.title)
    {
        score += 40.0;
        reasons.push("title=exact".into());
    } else if base_title(&title) == base_title(&wanted.title) {
        score += 35.0;
        reasons.push("title=base".into());
    } else {
        let sim = similarity(&base_title(&title), &base_title(&wanted.title));
        if sim >= 0.6 {
            score += 25.0 * sim;
        }
        reasons.push(format!("title~{sim:.2}"));
    }
    let artist_share = artists_match(&wanted.artists, Credits::List(&meta_artists(cand)));
    score += 35.0 * artist_share;
    reasons.push(format!("artists={artist_share:.2}"));
    let album = as_str(cand.get("album"));
    match &wanted.album {
        Some(w) if !w.is_empty() => {
            if normalize(&album) == normalize(w) {
                score += 15.0;
                reasons.push("album".into());
            }
        }
        _ => score += 7.5, // unknown album: neutral half credit
    }
    match (
        wanted.duration(),
        as_f64(cand.get("duration")).filter(|d| *d != 0.0),
    ) {
        (Some(w), Some(c)) => {
            let diff = (c - w).abs();
            if diff <= 3.0 {
                score += 10.0;
                reasons.push("duration".into());
            } else if diff > 15.0 {
                score -= 10.0;
                reasons.push("duration!".into());
            }
        }
        _ => score += 5.0,
    }
    let wanted_markers = wanted.markers();
    let extra: Vec<_> = markers(&full_title)
        .difference(&wanted_markers)
        .copied()
        .collect();
    if !extra.is_empty() {
        score -= 25.0 * extra.len() as f64;
        reasons.push(format!("version:{}", extra.join(",")));
    }
    if normalize(&album) == normalize(&title) && wanted.album.is_none() {
        // A single named like the track: prefer the album release when both exist.
        score -= 3.0;
        reasons.push("single".into());
    }
    Scored {
        score: round2(score),
        reasons,
        rejected: None,
    }
}

/// Remove `name` from `text` where it stands as a whole word (case-insensitive).
fn strip_name(text: &str, name: &str) -> String {
    let name = name.trim();
    // A one-letter artist ("A") would otherwise erase that letter from the whole title.
    if name.chars().count() < 2 {
        return text.to_string();
    }
    let mut pattern = String::from("(?i)");
    if name.starts_with(|c: char| c.is_alphanumeric()) {
        pattern.push_str(r"\b");
    }
    pattern.push_str(&regex::escape(name));
    if name.ends_with(|c: char| c.is_alphanumeric()) {
        pattern.push_str(r"\b");
    }
    match Regex::new(&pattern) {
        Ok(r) => r.replace_all(text, " ").into_owned(),
        Err(_) => text.to_string(),
    }
}

/// Score an audio source (YouTube/SoundCloud entry) for a known catalog track.
///
/// Hard rejections protect the library: wrong duration, a version marker the
/// track does not have (instrumental, karaoke, sped up ...), or a missing artist.
pub fn score_source_candidate(
    wanted: &Wanted,
    title: &str,
    uploader: &str,
    duration: Option<f64>,
    official: bool,
    artists: &[String],
    tolerance: f64,
) -> Scored {
    let duration = duration.filter(|d| *d != 0.0);
    let duration_score = match (wanted.duration(), duration) {
        (Some(w), Some(d)) => {
            let diff = (d - w).abs();
            if diff > tolerance {
                return reject(format!("duration {d:.0}s vs {w:.0}s"), "duration");
            }
            20.0 * (1.0 - diff / tolerance)
        }
        _ => 5.0,
    };
    let wanted_markers = wanted.markers();
    let extra: Vec<_> = markers(title)
        .difference(&wanted_markers)
        .copied()
        .filter(|m| *m != "extended")
        .collect();
    if !extra.is_empty() {
        return reject(format!("version:{}", extra.join(",")), "version");
    }
    let uploader_clean = TOPIC_SUFFIX.replace(uploader, "").into_owned();
    let mut credits = artists.to_vec();
    credits.push(uploader_clean.clone());
    let mut artist_share = artists_match(&wanted.artists, Credits::List(&credits));
    if artist_share == 0.0 {
        // Videos by non-official channels often embed "Artist - Title" in the title.
        let mut from_title = split_artists(title.split(" - ").next().unwrap_or(""));
        from_title.push(title.to_string());
        artist_share = artists_match(&wanted.artists, Credits::List(&from_title));
    }
    if artist_share == 0.0 {
        return reject("artist missing".into(), "artist");
    }
    let mut clean_title = VIDEO_NOISE.replace_all(title, " ").into_owned();
    for name in wanted
        .artists
        .iter()
        .chain(std::iter::once(&uploader_clean))
    {
        clean_title = strip_name(&clean_title, name);
    }
    let clean_title = EDGE_DASHES.replace_all(&clean_title, " ").into_owned();
    let wanted_base = base_title(&wanted.title);
    let mut title_sim = similarity(&base_title(&clean_title), &wanted_base)
        .max(similarity(&base_title(title), &wanted_base));
    let wanted_words = words(&wanted_base);
    if !wanted_words.is_empty() && wanted_words.is_subset(&words(title)) {
        title_sim = title_sim.max(0.9);
    }
    if title_sim < 0.5 {
        return reject(format!("title~{title_sim:.2}"), "title");
    }
    if wanted.duration().is_none()
        && !title_matches(&wanted.title, &clean_title, 0.8)
        && !title_matches(&wanted.title, title, 0.8)
    {
        // No duration to confirm it (a plain-text import): the title has to be the same.
        return reject(format!("title~{title_sim:.2}"), "title");
    }
    let mut score = 45.0 * title_sim + 25.0 * artist_share + duration_score;
    let mut reasons = vec![
        format!("title~{title_sim:.2}"),
        format!("artists={artist_share:.2}"),
    ];
    if official || uploader.to_lowercase().ends_with(" - topic") {
        score += 10.0;
        reasons.push("official".into());
    }
    Scored {
        score: round2(score),
        reasons,
        rejected: None,
    }
}

/// Whether `got` names the same song as `wanted` (not merely a similar title).
///
/// Used where no duration can confirm a match (plain-text imports): "25 (Interlude)"
/// is not "15 (Interlude)", and "Расстройство" is not "Временно".
pub fn title_matches(wanted: &str, got: &str, strict: f64) -> bool {
    let numbers = |s: &str| -> HashSet<String> {
        NUMBER
            .find_iter(&normalize(s))
            .map(|m| m.as_str().to_string())
            .collect()
    };
    if !numbers(wanted).is_subset(&numbers(got)) {
        return false;
    }
    let (a, b) = (base_title(wanted), base_title(got));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let wa: HashSet<&str> = a.split(' ').collect();
    let wb: HashSet<&str> = b.split(' ').collect();
    if wa.is_subset(&wb) && wb.difference(&wa).count() <= 3 {
        return true;
    }
    similarity(&a, &b).max(ratio(&latin(&a), &latin(&b))) >= strict
}

/// Share of query words found in the fields (prefix match, for search-as-you-type).
pub fn relevance(query: &str, fields: &[&str]) -> f64 {
    let wanted: Vec<String> = normalize(query)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect();
    if wanted.is_empty() {
        return 0.0;
    }
    let joined: Vec<&str> = fields.iter().copied().filter(|f| !f.is_empty()).collect();
    let have_text = normalize(&joined.join(" "));
    let have: Vec<&str> = have_text.split(' ').filter(|w| !w.is_empty()).collect();
    let hits = wanted
        .iter()
        .filter(|w| {
            have.iter()
                .any(|h| h == w || (w.chars().count() >= 2 && h.starts_with(w.as_str())))
        })
        .count();
    hits as f64 / wanted.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(v: Value) -> Json {
        v.as_object().cloned().unwrap()
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn src(w: &Wanted, title: &str, uploader: &str, duration: Option<f64>) -> Scored {
        score_source_candidate(w, title, uploader, duration, false, &[], 10.0)
    }

    #[test]
    fn normalize_handles_cyrillic_case_and_punctuation() {
        assert_eq!(normalize("  Ёлки — «Прованс»!! "), "елки прованс");
        assert_eq!(normalize("Beyoncé"), "beyonce");
        assert_eq!(normalize("Straße"), "strasse");
        assert_eq!(
            normalize("Йога"),
            "иога",
            "NFKD drops the breve like Python"
        );
    }

    #[test]
    fn split_artists_cases() {
        let cases: &[(&str, &[&str])] = &[
            ("TVETH, BATO", &["TVETH", "BATO"]),
            ("9mice & Kai Angel", &["9mice", "Kai Angel"]),
            ("Big Baby Tape feat. Kizaru", &["Big Baby Tape", "Kizaru"]),
            ("OG Buda x Mayot", &["OG Buda", "Mayot"]),
            ("Баста, Моя Мишель", &["Баста", "Моя Мишель"]),
            ("Daft Punk", &["Daft Punk"]),
        ];
        for (credit, expected) in cases {
            assert_eq!(split_artists(credit), s(expected), "{credit}");
        }
    }

    #[test]
    fn markers_cases() {
        let cases: &[(&str, &[&str])] = &[
            ("Black Water (минус)", &["instrumental"]),
            ("Black Water Instrumental", &["instrumental"]),
            ("Порезы на руках (sped up)", &["speed"]),
            ("Song - Slowed + Reverb", &["speed"]),
            ("Radioactive (Live)", &["live"]),
            ("Караоке версия", &["karaoke"]),
            ("Black Water", &[]),
            ("Deliver (Beatdown)", &[]),
            ("Life Is Live", &["live"]),
            ("i can't feel my face x les _ tiktok version", &["tiktok"]),
            ("les. [slowed + best part + looped]", &["loop", "speed"]),
        ];
        for (title, expected) in cases {
            assert_eq!(
                markers(title),
                expected.iter().copied().collect(),
                "{title}"
            );
        }
    }

    #[test]
    fn sequence_matcher_ratio_matches_difflib() {
        assert_eq!(ratio("abcd", "bcde"), 0.75);
        assert!((ratio("hella quality", "hella qualty") - 0.96).abs() < 0.001);
        assert_eq!(ratio("", ""), 1.0);
        assert_eq!(ratio("abc", "xyz"), 0.0);
    }

    #[test]
    fn base_title_drops_feat_and_brackets() {
        assert_eq!(base_title("Hella Quality (feat. Bato)"), "hella quality");
        assert_eq!(
            base_title("Instant Crush (feat. Julian Casablancas)"),
            "instant crush"
        );
    }

    #[test]
    fn catalog_scoring_prefers_exact_collab_and_penalizes_versions() {
        let wanted = Wanted::new("Hella Quality", &["TVETH", "BATO"]);
        let good = m(
            json!({"title": "Hella Quality", "artists": ["TVETH", "Bato"], "album": "RAW", "duration": 214}),
        );
        let solo = m(
            json!({"title": "Hella Quality", "artists": ["TVETH"], "album": "RAW", "duration": 214}),
        );
        let remix = m(
            json!({"title": "Hella Quality (Remix)", "artists": ["TVETH", "Bato"], "album": "R", "duration": 200}),
        );
        let (g, so, r) = (
            score_catalog_candidate(&wanted, &good).score,
            score_catalog_candidate(&wanted, &solo).score,
            score_catalog_candidate(&wanted, &remix).score,
        );
        assert!(g > so && so > r, "{g} {so} {r}");
        assert!(g >= 80.0);
    }

    #[test]
    fn catalog_scoring_album_and_isrc() {
        let wanted = Wanted {
            album: Some("Night Visions".into()),
            ..Wanted::new("Radioactive", &["Imagine Dragons"])
        };
        let album = m(
            json!({"title": "Radioactive", "artists": ["Imagine Dragons"], "album": "Night Visions", "duration": 187}),
        );
        let single = m(
            json!({"title": "Radioactive", "artists": ["Imagine Dragons"], "album": "Radioactive", "duration": 276}),
        );
        assert!(
            score_catalog_candidate(&wanted, &album).score
                > score_catalog_candidate(&wanted, &single).score
        );
        let by_isrc = Wanted {
            isrc: Some("usum1".into()),
            ..Wanted::new("x", &["y"])
        };
        assert_eq!(
            score_catalog_candidate(&by_isrc, &m(json!({"isrc": "USUM1"}))).score,
            100.0
        );
    }

    #[test]
    fn catalog_scoring_without_album_prefers_album_release_over_single() {
        let wanted = Wanted::new("Radioactive", &["Imagine Dragons"]);
        let album = m(
            json!({"title": "Radioactive", "artists": ["Imagine Dragons"], "album": "Night Visions"}),
        );
        let single = m(
            json!({"title": "Radioactive", "artists": ["Imagine Dragons"], "album": "Radioactive"}),
        );
        assert!(
            score_catalog_candidate(&wanted, &album).score
                > score_catalog_candidate(&wanted, &single).score
        );
    }

    #[test]
    fn source_scoring_rejects_minus_wrong_duration_and_wrong_artist() {
        let wanted = Wanted {
            duration: Some(177.0),
            ..Wanted::new("Black Water", &["FORTUNA 812"])
        };
        let ok = src(&wanted, "Black Water", "FORTUNA 812 - Topic", Some(178.0));
        assert!(ok.rejected.is_none() && ok.score > 70.0, "{ok:?}");
        assert!(
            src(
                &wanted,
                "FORTUNA 812 - Black Water (минус)",
                "beats",
                Some(177.0)
            )
            .rejected
            .is_some()
        );
        assert_eq!(
            src(&wanted, "Black Water", "FORTUNA 812 - Topic", Some(240.0)).rejected,
            Some("duration")
        );
        assert_eq!(
            src(&wanted, "Black Water", "Doobie Brothers", Some(177.0)).rejected,
            Some("artist")
        );
    }

    #[test]
    fn source_scoring_reads_artist_from_video_title_and_prefers_topic() {
        let wanted = Wanted {
            duration: Some(124.0),
            ..Wanted::new("Порезы на руках", &["Юпи"])
        };
        let video = src(
            &wanted,
            "Юпи - Порезы на руках (Official Video)",
            "Random Uploads",
            Some(125.0),
        );
        let topic = src(&wanted, "Порезы на руках", "Юпи - Topic", Some(124.0));
        assert!(video.rejected.is_none() && topic.rejected.is_none());
        assert!(topic.score > video.score);
    }

    #[test]
    fn source_scoring_allows_version_present_in_request() {
        let wanted = Wanted {
            duration: Some(300.0),
            ..Wanted::new("Song (Live)", &["Band"])
        };
        assert!(
            src(&wanted, "Band - Song (Live)", "Band", Some(301.0))
                .rejected
                .is_none()
        );
    }

    #[test]
    fn one_letter_artist_does_not_erase_the_title() {
        let wanted = Wanted {
            duration: Some(200.0),
            ..Wanted::new("Ave Maria", &["A"])
        };
        let scored = src(&wanted, "Ave Maria", "A - Topic", Some(200.0));
        assert!(scored.rejected.is_none(), "{scored:?}");
    }

    #[test]
    fn find_name_ignores_separators_alphabet_and_spacing() {
        let hits: &[(&str, &str, usize)] = &[
            (
                "Elox1m & FORTUNA 812 -- Порезы на руках (slowed)",
                "FORTUNA 812",
                1,
            ),
            ("фортуна⋆812 - Рассвет", "FORTUNA 812", 0),
            ("СНОВа.-фортуна812", "FORTUNA 812", 1),
            ("Кошмар — FORTUNA 812", "FORTUNA 812", 1),
            (
                "reflections the neighbourhood slowed",
                "The Neighbourhood",
                1,
            ),
            ("Here-Alessia Cara (Slowed Down)", "Alessia Cara", 1),
            ("childish Gabino- “oh girl”", "Childish Gambino", 0),
            ("𝙝𝙤𝙪𝙨𝙚 𝙤𝙛 𝙗𝙖𝙡𝙡𝙤𝙤𝙣𝙨 - 𝙨𝙡𝙤𝙬𝙚𝙙", "House of Balloons", 0),
        ];
        for (text, name, at) in hits {
            assert_eq!(find_name(text, name), Some(*at), "{name} in {text}");
        }
        for (text, name) in [
            ("The Neighbors - Sweater Weather", "The Neighbourhood"),
            ("lez - slowed", "les"), // no typo allowance in short words
            ("fortunate", "FORTUNA"),
            ("anything", ""),
        ] {
            assert_eq!(find_name(text, name), None, "{name} in {text}");
        }
    }

    #[test]
    fn search_text_drops_noise_and_version_words() {
        let t = "Russ - 3:15 (Breathe) ~ Slowed + Reverb More (3:15 AM version)";
        assert_eq!(search_text(t, false), "russ 3 15 more");
        assert_eq!(
            search_text("FORTUNA 812 - Black Water (Official Audio) [sped up]", true),
            "fortuna 812 black water up"
        );
        assert_eq!(
            search_text("les. [slowed + best part + looped]", false),
            "les"
        );
    }

    #[test]
    fn segments_split_coarse_then_fine() {
        assert_eq!(
            segments("Elox1m & FORTUNA 812 -- Порезы на руках (slowed)"),
            ["Elox1m", "FORTUNA 812", "Порезы на руках"]
        );
        assert_eq!(
            segments("фортуна⋆812 - Рассвет"),
            ["фортуна⋆812", "Рассвет", "фортуна", "812"]
        );
        assert_eq!(
            segments("фортуна812-клуб88"),
            ["фортуна812-клуб88", "фортуна812", "клуб88"]
        );
    }

    #[test]
    fn relevance_supports_prefixes_and_cyrillic() {
        assert_eq!(relevance("юпи порезы", &["Юпи", "Порезы на руках"]), 1.0);
        assert_eq!(
            relevance(
                "юпи порезы",
                &["Естраден Оркестър", "Всичко започна през юни"]
            ),
            0.0
        );
        assert_eq!(
            relevance("big ba", &["Big Baby Tape", "Gimme The Loot"]),
            1.0
        );
        assert_eq!(
            relevance("big baby tape kizaru", &["Big Baby Tape", "Bandana"]),
            0.75
        );
    }

    #[test]
    fn artists_match_across_alphabets() {
        let am = |w: &str, c: &str| artists_match(&[w.to_string()], Credits::Text(c));
        assert_eq!(am("Братья Грим", "Bratya Grim"), 1.0);
        assert_eq!(am("МС Сенечка", "MC Senechka, Say Shu"), 1.0);
        assert_eq!(am("АК-47", "AK-47"), 1.0);
        assert_eq!(am("Сектор Газа", "Russian Lo-Fi Radio"), 0.0);
        assert_eq!(am("DK", "Экспайн"), 0.0);
        assert_eq!(am("MC", "M.C."), 1.0);
    }

    #[test]
    fn import_sources_need_the_same_title() {
        let cases: &[(&str, &[&str], &str, &str, bool)] = &[
            (
                "25 (Interlude)",
                &["Кишлак"],
                "15 (Interlude)",
                "Кишлак",
                false,
            ),
            ("BARBIE", &["GONE.Fludd"], "RARI LOVE", "GONE.Fludd", false),
            ("Ламбада", &["N1NT3ND0"], "Пуля за брата", "N1NT3ND0", false),
            (
                "Вуаля",
                &["Мэйби Бэйби", "Дора"],
                "МЭЙБИ БЭЙБИ, дора — Вуаля (1 час)",
                "fz1k",
                false,
            ),
            (
                "awake",
                &["russianmafiaextra"],
                "russianmafiaextra x cowboyclicker – awake",
                "cowboyclick",
                true,
            ),
            (
                "きみに回帰線",
                &["Inabakumori"],
                "稲葉曇『きみに回帰線』Vo. 歌愛ユキ",
                "inabakumori",
                true,
            ),
            (
                "Деструктив [Бонус]",
                &["DK", "RAM"],
                "Диструктив [Бонус] (feat. RAM)",
                "DK",
                true,
            ),
        ];
        for (title, artists, video, uploader, ok) in cases {
            let scored = src(&Wanted::new(title, artists), video, uploader, None);
            assert_eq!(
                scored.rejected.is_none(),
                *ok,
                "{title} vs {video}: {scored:?}"
            );
        }
    }

    #[test]
    fn title_matches_cases() {
        assert!(title_matches("Пора домой", "Pora Domoy", 0.8));
        assert!(title_matches(
            "Hella Quality",
            "Hella Quality (feat. Bato)",
            0.8
        ));
        assert!(!title_matches("Расстройство", "Временно", 0.8));
        assert!(!title_matches("25 (Interlude)", "15 (Interlude)", 0.8));
    }
}
