import pytest

from navidrome_proxy import matching as m


def test_normalize_handles_cyrillic_case_and_punctuation():
    assert m.normalize("  Ёлки — «Прованс»!! ") == "елки прованс"
    assert m.normalize("Beyoncé") == "beyonce"


@pytest.mark.parametrize(
    ("credit", "expected"),
    [
        ("TVETH, BATO", ["TVETH", "BATO"]),
        ("9mice & Kai Angel", ["9mice", "Kai Angel"]),
        ("Big Baby Tape feat. Kizaru", ["Big Baby Tape", "Kizaru"]),
        ("OG Buda x Mayot", ["OG Buda", "Mayot"]),
        ("Баста, Моя Мишель", ["Баста", "Моя Мишель"]),
        ("Daft Punk", ["Daft Punk"]),
    ],
)
def test_split_artists(credit, expected):
    assert m.split_artists(credit) == expected


@pytest.mark.parametrize(
    ("title", "expected"),
    [
        ("Black Water (минус)", {"instrumental"}),
        ("Black Water Instrumental", {"instrumental"}),
        ("Порезы на руках (sped up)", {"speed"}),
        ("Song - Slowed + Reverb", {"speed"}),
        ("Radioactive (Live)", {"live"}),
        ("Караоке версия", {"karaoke"}),
        ("Black Water", set()),
        ("Deliver (Beatdown)", set()),
        ("Life Is Live", {"live"}),
    ],
)
def test_markers(title, expected):
    assert m.markers(title) == expected


def test_base_title_drops_feat_and_brackets():
    assert m.base_title("Hella Quality (feat. Bato)") == "hella quality"
    assert m.base_title("Instant Crush (feat. Julian Casablancas)") == "instant crush"


def test_catalog_scoring_prefers_exact_collab_and_penalizes_versions():
    wanted = m.Wanted("Hella Quality", ["TVETH", "BATO"])
    good = {"title": "Hella Quality", "artists": ["TVETH", "Bato"], "album": "RAW", "duration": 214}
    solo = {"title": "Hella Quality", "artists": ["TVETH"], "album": "RAW", "duration": 214}
    remix = {"title": "Hella Quality (Remix)", "artists": ["TVETH", "Bato"], "album": "R", "duration": 200}
    s_good, s_solo, s_remix = (m.score_catalog_candidate(wanted, c).score for c in (good, solo, remix))
    assert s_good > s_solo > s_remix
    assert s_good >= 80


def test_catalog_scoring_album_and_isrc():
    wanted = m.Wanted("Radioactive", ["Imagine Dragons"], album="Night Visions")
    album_version = {
        "title": "Radioactive",
        "artists": ["Imagine Dragons"],
        "album": "Night Visions",
        "duration": 187,
    }
    single = {"title": "Radioactive", "artists": ["Imagine Dragons"], "album": "Radioactive", "duration": 276}
    assert (
        m.score_catalog_candidate(wanted, album_version).score
        > m.score_catalog_candidate(wanted, single).score
    )
    by_isrc = m.score_catalog_candidate(m.Wanted("x", ["y"], isrc="usum1"), {"isrc": "USUM1"})
    assert by_isrc.score == 100


def test_catalog_scoring_without_album_prefers_album_release_over_single():
    wanted = m.Wanted("Radioactive", ["Imagine Dragons"])
    album_version = {"title": "Radioactive", "artists": ["Imagine Dragons"], "album": "Night Visions"}
    single = {"title": "Radioactive", "artists": ["Imagine Dragons"], "album": "Radioactive"}
    assert (
        m.score_catalog_candidate(wanted, album_version).score
        > m.score_catalog_candidate(wanted, single).score
    )


def test_source_scoring_rejects_minus_wrong_duration_and_wrong_artist():
    wanted = m.Wanted("Black Water", ["FORTUNA 812"], duration=177)
    ok = m.score_source_candidate(wanted, "Black Water", "FORTUNA 812 - Topic", 178)
    assert ok.rejected is None and ok.score > 70
    assert m.score_source_candidate(wanted, "FORTUNA 812 - Black Water (минус)", "beats", 177).rejected
    assert m.score_source_candidate(wanted, "Black Water", "FORTUNA 812 - Topic", 240).rejected == "duration"
    assert m.score_source_candidate(wanted, "Black Water", "Doobie Brothers", 177).rejected == "artist"


def test_source_scoring_reads_artist_from_video_title_and_prefers_topic():
    wanted = m.Wanted("Порезы на руках", ["Юпи"], duration=124)
    video = m.score_source_candidate(wanted, "Юпи - Порезы на руках (Official Video)", "Random Uploads", 125)
    topic = m.score_source_candidate(wanted, "Порезы на руках", "Юпи - Topic", 124)
    assert video.rejected is None and topic.rejected is None
    assert topic.score > video.score


def test_source_scoring_allows_version_present_in_request():
    wanted = m.Wanted("Song (Live)", ["Band"], duration=300)
    assert m.score_source_candidate(wanted, "Band - Song (Live)", "Band", 301).rejected is None
