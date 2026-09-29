import json
import xml.etree.ElementTree as ET

from navidrome_proxy.subsonic import response as r

IDENTITY = r.ServerIdentity(server_version="0.63.2 (be10f89c)")
NS = "{http://subsonic.org/restapi}"


def test_json_envelope_matches_navidrome_shape():
    body, ctype = r.ok(IDENTITY, {"f": "json"}, {"song": {"id": "x", "isDir": False}})
    assert ctype.startswith("application/json")
    root = json.loads(body)["subsonic-response"]
    assert root == {
        "status": "ok",
        "version": "1.16.1",
        "type": "navidrome",
        "serverVersion": "0.63.2 (be10f89c)",
        "openSubsonic": True,
        "song": {"id": "x", "isDir": False},
    }


def test_xml_is_the_default_and_nests_lists_and_objects():
    payload = {
        "searchResult3": {
            "song": [
                {"id": "1", "isDir": False, "genres": [{"name": "Rap"}], "replayGain": {}, "isrc": ["A"]},
                {"id": "2", "isDir": False},
            ]
        }
    }
    body, ctype = r.ok(IDENTITY, {}, payload)
    assert ctype.startswith("application/xml")
    root = ET.fromstring(body)
    assert root.tag == f"{NS}subsonic-response"
    assert root.get("openSubsonic") == "true"
    songs = root.find(f"{NS}searchResult3").findall(f"{NS}song")
    assert [s.get("id") for s in songs] == ["1", "2"]
    assert songs[0].get("isDir") == "false"
    assert songs[0].find(f"{NS}genres").get("name") == "Rap"
    assert songs[0].find(f"{NS}isrc").text == "A"


def test_error_is_status_failed_with_code():
    body, _ = r.error(IDENTITY, {"f": "json"}, r.NOT_FOUND, "Song not found")
    root = json.loads(body)["subsonic-response"]
    assert root["status"] == "failed"
    assert root["error"] == {"code": 70, "message": "Song not found"}


def test_jsonp_wraps_callback():
    body, ctype = r.ok(IDENTITY, {"f": "jsonp", "callback": "cb"})
    assert body.startswith(b"cb(") and body.endswith(b");")
    assert "javascript" in ctype


def test_identity_updates_from_upstream_ping():
    identity = r.ServerIdentity()
    identity.update_from(
        {"version": "1.16.1", "type": "navidrome", "serverVersion": "0.64.0", "openSubsonic": True}
    )
    assert identity.server_version == "0.64.0"
