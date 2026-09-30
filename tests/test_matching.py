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


def test_relevance_supports_prefixes_and_cyrillic():
    assert m.relevance("юпи порезы", "Юпи", "Порезы на руках") == 1.0
    assert m.relevance("юпи порезы", "Естраден Оркестър", "Всичко започна през юни") == 0.0
    assert m.relevance("big ba", "Big Baby Tape", "Gimme The Loot") == 1.0
    assert m.relevance("big baby tape kizaru", "Big Baby Tape", "Bandana") == 0.75


def test_artists_match_across_alphabets():
    assert m.artists_match(["Братья Грим"], "Bratya Grim") == 1
    assert m.artists_match(["МС Сенечка"], "MC Senechka, Say Shu") == 1
    assert m.artists_match(["АК-47"], "AK-47") == 1
    assert m.artists_match(["Сектор Газа"], "Russian Lo-Fi Radio") == 0
    assert m.artists_match(["DK"], "Экспайн") == 0


@pytest.mark.parametrize(
    ("title", "artists", "video", "uploader", "ok"),
    [
        ("25 (Interlude)", ["Кишлак"], "15 (Interlude)", "Кишлак", False),
        ("BARBIE", ["GONE.Fludd"], "RARI LOVE", "GONE.Fludd", False),
        ("Ламбада", ["N1NT3ND0"], "Пуля за брата", "N1NT3ND0", False),
        ("Вуаля", ["Мэйби Бэйби", "Дора"], "МЭЙБИ БЭЙБИ, дора — Вуаля (1 час)", "fz1k", False),
        ("awake", ["russianmafiaextra"], "russianmafiaextra x cowboyclicker – awake", "cowboyclick", True),
        ("きみに回帰線", ["Inabakumori"], "稲葉曇『きみに回帰線』Vo. 歌愛ユキ", "inabakumori", True),
        ("Деструктив [Бонус]", ["DK", "RAM"], "Диструктив [Бонус] (feat. RAM)", "DK", True),
    ],
)
def test_import_sources_need_the_same_title(title, artists, video, uploader, ok):
    """Real lines of an imported list; no duration is known, so the title decides."""
    scored = m.score_source_candidate(m.Wanted(title, artists), video, uploader, None)
    assert (scored.rejected is None) is ok


def test_title_matches():
    assert m.title_matches("Пора домой", "Pora Domoy")
    assert m.title_matches("Hella Quality", "Hella Quality (feat. Bato)")
    assert not m.title_matches("Расстройство", "Временно")
    assert not m.title_matches("25 (Interlude)", "15 (Interlude)")
