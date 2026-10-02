import json

import pytest

from navidrome_proxy import __main__ as cli
from navidrome_proxy import db
from navidrome_proxy.db import OwnerLock, Store


@pytest.fixture
def env(tmp_path, monkeypatch):
    music, data = tmp_path / "music", tmp_path / "data"
    music.mkdir()
    monkeypatch.setenv("MUSIC_DIR", str(music))
    monkeypatch.setenv("DATA_DIR", str(data))
    return music, data


def run(capsys, *argv):
    cli.main(list(argv))
    return json.loads(capsys.readouterr().out)


def test_check_db_and_relayout_dry_run(env, capsys):
    assert run(capsys, "check-db") == {"ok": True, "problems": []}
    assert run(capsys, "relayout", "--dry-run") == {"moved": 0, "kept": 0, "missing": 0}


def test_remux_dash_needs_no_database(env, capsys):
    _, data = env
    assert run(capsys, "remux-dash", "--dry-run")["checked"] == 0
    assert not (data / "proxy.sqlite3").exists()


def test_a_second_process_is_refused(env):
    _, data = env
    with OwnerLock(data / "proxy.sqlite3"), pytest.raises(SystemExit, match="held by another"):
        cli.main(["check-db"])


def test_the_lock_is_released_after_a_command(env, capsys):
    _, data = env
    run(capsys, "check-db")
    with OwnerLock(data / "proxy.sqlite3"):
        pass


def test_a_newer_database_is_refused_with_a_message(env):
    _, data = env
    store = Store(data / "proxy.sqlite3")
    store.set_meta("schema_version", str(db.SCHEMA_VERSION + 1))
    store.close()
    with pytest.raises(SystemExit, match="schema version"):
        cli.main(["check-db"])


def test_recheck_of_an_unknown_import(env):
    with pytest.raises(SystemExit, match="no import nope"):
        cli.main(["recheck-import", "nope"])
