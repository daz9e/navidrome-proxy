"""Text normalization and candidate scoring shared by search, sources and imports."""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass, field
from difflib import SequenceMatcher

# Words that mark a *different recording* than the original. A candidate carrying
# one that the request does not carry is rejected for downloads (the "minus instead
# of the original" incident with FORTUNA 812 — Black Water) and penalized in imports.
VERSION_MARKERS: dict[str, tuple[str, ...]] = {
    "instrumental": (
        "instrumental",
        "инструментал",
        "минус",
        "minus",
        "backing track",
        "type beat",
        "beat",
        "no vocals",
        "без слов",
    ),
    "karaoke": ("karaoke", "караоке"),
    "remix": ("remix", "ремикс", "rmx", "flip", "bootleg", "mashup", "мэшап", "vip mix"),
    "live": ("live", "лайв", "концерт", "concert", "unplugged"),
    "speed": (
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
    ),
    "cover": ("cover", "кавер", "tribute", "в стиле", "originally performed", "piano version"),
    "acoustic": ("acoustic", "акустик"),
    "extended": ("extended", "club mix", "radio edit", "edit"),
    "demo": ("demo", "демо", "snippet", "сниппет", "preview", "leak"),
}

# Noise that video titles add around the real title.
_VIDEO_NOISE = re.compile(
    r"\b(official( music)?( video| audio| lyric video| visualizer)?|music video|lyric video|lyrics?|"
    r"audio|video|clip|клип|премьера|premiere|hd|hq|4k|mv|visualizer|текст песни|полная версия|full version|"
    r"prod\.? by [^()\[\]]*)\b",
    re.IGNORECASE,
)
_FEAT = re.compile(r"[\(\[]?\s*\b(feat\.?|ft\.?|featuring|при уч\.?|при участии)\s+[^()\[\]]*[\)\]]?", re.I)
_ARTIST_SPLIT = re.compile(
    r"\s*(?:,|;|&|/|\+|\bfeat\.?|\bft\.?|\bfeaturing|\bx\b|\bх\b|\band\b|\bи\b|\bwith\b)\s*", re.IGNORECASE
)
_BRACKETS = re.compile(r"[\(\[\{][^\)\]\}]*[\)\]\}]")


def normalize(text: str | None) -> str:
    """Case/diacritics/punctuation-insensitive form used for comparisons."""
    value = unicodedata.normalize("NFKC", str(text or "")).casefold()
    value = value.replace("ё", "е").replace("’", "'").replace("`", "'")
    value = "".join(ch for ch in unicodedata.normalize("NFKD", value) if not unicodedata.combining(ch))
    value = re.sub(r"[^\w\s]", " ", value)
    return " ".join(value.split())


_TRANSLIT = str.maketrans(
    {
        "а": "a",
        "б": "b",
        "в": "v",
        "г": "g",
        "д": "d",
        "е": "e",
        "ж": "zh",
        "з": "z",
        "и": "i",
        "й": "y",
        "к": "k",
        "л": "l",
        "м": "m",
        "н": "n",
        "о": "o",
        "п": "p",
        "р": "r",
        "с": "s",
        "т": "t",
        "у": "u",
        "ф": "f",
        "х": "h",
        "ц": "ts",
        "ч": "ch",
        "ш": "sh",
        "щ": "sch",
        "ъ": "",
        "ы": "y",
        "ь": "",
        "э": "e",
        "ю": "yu",
        "я": "ya",
    }
)


def latin(text: str | None) -> str:
    """:func:`normalize` with Cyrillic transliterated, so "Братья Грим" meets "Bratya Grim"."""
    return normalize(text).translate(_TRANSLIT)


def words(text: str | None) -> set[str]:
    return set(normalize(text).split())


def split_artists(credit: str | None) -> list[str]:
    parts = [p.strip() for p in _ARTIST_SPLIT.split(str(credit or "")) if p and p.strip()]
    seen: set[str] = set()
    out = []
    for part in parts:
        key = normalize(part)
        if key and key not in seen:
            seen.add(key)
            out.append(part)
    return out


def base_title(title: str | None) -> str:
    """Title without featuring credits and bracketed annotations."""
    value = _FEAT.sub(" ", str(title or ""))
    stripped = _BRACKETS.sub(" ", value)
    return normalize(stripped) or normalize(value)


def markers(text: str | None) -> set[str]:
    norm = f" {normalize(text)} "
    found = set()
    for name, variants in VERSION_MARKERS.items():
        for variant in variants:
            v = normalize(variant)
            if v and (f" {v} " in norm or (len(v) >= 6 and v in norm)):
                found.add(name)
                break
    return found


def similarity(a: str | None, b: str | None) -> float:
    na, nb = normalize(a), normalize(b)
    if not na or not nb:
        return 0.0
    if na == nb:
        return 1.0
    return SequenceMatcher(None, na, nb).ratio()


def artists_match(wanted: list[str], candidate: list[str] | str) -> float:
    """Share of requested artists found among the candidate's credits (0..1)."""
    wanted_keys = [normalize(a) for a in wanted if normalize(a)]
    if not wanted_keys:
        return 1.0
    if isinstance(candidate, str):
        candidate = split_artists(candidate) + [candidate]
    have = [normalize(c) for c in candidate if normalize(c)]
    joined = " ".join(have)
    have_latin = [latin(c) for c in have]
    hits = 0
    for key in wanted_keys:
        same = key in have or f" {key} " in f" {joined} " or any(similarity(key, h) >= 0.88 for h in have)
        if same or any(similarity(latin(key), h) >= 0.88 for h in have_latin):
            hits += 1
    return hits / len(wanted_keys)


@dataclass
class Wanted:
    title: str
    artists: list[str]
    album: str | None = None
    duration: float | None = None
    isrc: str | None = None

    @property
    def markers(self) -> set[str]:
        return markers(self.title) | markers(self.album)


@dataclass
class Scored:
    score: float
    reasons: list[str] = field(default_factory=list)
    rejected: str | None = None


def score_catalog_candidate(wanted: Wanted, cand: dict) -> Scored:
    """Score a Deezer candidate for a free-text ``artist - title`` request.

    ``cand`` is proxy track metadata (see :func:`deezer.track_meta`).
    """
    reasons: list[str] = []
    if wanted.isrc and cand.get("isrc") and wanted.isrc.upper() == str(cand["isrc"]).upper():
        return Scored(100.0, ["isrc"])
    score = 0.0
    full_title = f"{cand.get('title', '')} {cand.get('title_version', '')}".strip()
    if normalize(full_title) == normalize(wanted.title) or normalize(cand.get("title")) == normalize(
        wanted.title
    ):
        score += 40
        reasons.append("title=exact")
    elif base_title(cand.get("title")) == base_title(wanted.title):
        score += 35
        reasons.append("title=base")
    else:
        sim = similarity(base_title(cand.get("title")), base_title(wanted.title))
        score += 25 * sim if sim >= 0.6 else 0
        reasons.append(f"title~{sim:.2f}")
    artist_share = artists_match(wanted.artists, cand.get("artists") or cand.get("artist") or "")
    score += 35 * artist_share
    reasons.append(f"artists={artist_share:.2f}")
    if wanted.album:
        if normalize(cand.get("album")) == normalize(wanted.album):
            score += 15
            reasons.append("album")
    else:
        score += 7.5  # unknown album: neutral half credit
    if wanted.duration and cand.get("duration"):
        diff = abs(float(cand["duration"]) - float(wanted.duration))
        if diff <= 3:
            score += 10
            reasons.append("duration")
        elif diff > 15:
            score -= 10
            reasons.append("duration!")
    else:
        score += 5
    extra = markers(full_title) - wanted.markers
    if extra:
        score -= 25 * len(extra)
        reasons.append("version:" + ",".join(sorted(extra)))
    if normalize(cand.get("album")) == normalize(cand.get("title")) and wanted.album is None:
        # A single named like the track: prefer the album release when both exist.
        score -= 3
        reasons.append("single")
    return Scored(round(score, 2), reasons)


def score_source_candidate(
    wanted: Wanted,
    title: str,
    uploader: str,
    duration: float | None,
    *,
    official: bool = False,
    artists: list[str] | None = None,
    tolerance: float = 10.0,
) -> Scored:
    """Score an audio source (YouTube/SoundCloud entry) for a known catalog track.

    Hard rejections protect the library: wrong duration, a version marker the
    track does not have (instrumental, karaoke, sped up ...), or a missing artist.
    """
    reasons: list[str] = []
    if wanted.duration and duration:
        diff = abs(float(duration) - float(wanted.duration))
        if diff > tolerance:
            return Scored(0, [f"duration {duration:.0f}s vs {wanted.duration:.0f}s"], "duration")
        duration_score = 20 * (1 - diff / tolerance)
    else:
        duration_score = 5.0
    extra = (markers(title) - wanted.markers) - {"extended"}
    if extra:
        return Scored(0, ["version:" + ",".join(sorted(extra))], "version")
    uploader_clean = re.sub(r"\s*-\s*topic$", "", uploader or "", flags=re.I)
    credits = list(artists or []) + [uploader_clean]
    artist_share = artists_match(wanted.artists, credits)
    if artist_share == 0:
        # Videos by non-official channels often embed "Artist - Title" in the title.
        artist_share = artists_match(wanted.artists, split_artists(title.split(" - ")[0]) + [title])
    if artist_share == 0:
        return Scored(0, ["artist missing"], "artist")
    clean_title = _VIDEO_NOISE.sub(" ", title)
    for name in wanted.artists + [uploader_clean]:
        clean_title = re.sub(re.escape(name), " ", clean_title, flags=re.I)
    clean_title = re.sub(r"^\s*[-–—:|]+|[-–—:|]+\s*$", " ", clean_title)
    title_sim = max(
        similarity(base_title(clean_title), base_title(wanted.title)),
        similarity(base_title(title), base_title(wanted.title)),
    )
    wanted_words = words(base_title(wanted.title))
    if wanted_words and wanted_words <= words(title):
        title_sim = max(title_sim, 0.9)
    if title_sim < 0.5:
        return Scored(0, [f"title~{title_sim:.2f}"], "title")
    score = 45 * title_sim + 25 * artist_share + duration_score
    reasons += [f"title~{title_sim:.2f}", f"artists={artist_share:.2f}"]
    if official or uploader.lower().endswith(" - topic"):
        score += 10
        reasons.append("official")
    return Scored(round(score, 2), reasons)


def relevance(query: str, *fields: str | None) -> float:
    """Share of query words found in the fields (prefix match, for search-as-you-type)."""
    wanted = normalize(query).split()
    if not wanted:
        return 0.0
    have = normalize(" ".join(f for f in fields if f)).split()
    hits = 0
    for word in wanted:
        if any(h == word or (len(word) >= 2 and h.startswith(word)) for h in have):
            hits += 1
    return hits / len(wanted)
