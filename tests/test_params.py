from navidrome_proxy.subsonic.params import Params, redact


def test_query_and_form_body_are_merged_in_order():
    p = Params.parse("u=alice&id=1&id=2&f=json", "id=3&v=1.16.1")
    assert p.getall("id") == ["1", "2", "3"]
    assert p.get("v") == "1.16.1"
    assert p.username == "alice"


def test_replace_keeps_position_and_can_drop():
    p = Params.parse("u=a&id=deezer:1&id=L2&f=json")
    assert p.replace("id", ["L1", "L2"]).encode() == "u=a&id=L1&id=L2&f=json"
    assert p.replace("id", []).encode() == "u=a&f=json"
    assert p.replace("albumId", ["X"]).getall("albumId") == ["X"]


def test_credentials_only_keeps_auth_and_protocol():
    p = Params.parse("u=a&t=tok&s=salt&v=1.16.1&c=Arpeggi&f=json&id=1")
    assert p.credentials().simple() == {"u": "a", "t": "tok", "s": "salt", "v": "1.16.1", "c": "Arpeggi"}
    assert p.has_credentials
    assert not Params.parse("u=a&t=tok").has_credentials


def test_int_parsing_falls_back():
    p = Params.parse("songCount=50&songOffset=abc")
    assert p.int("songCount", 20) == 50
    assert p.int("songOffset", 0) == 0
    assert p.int("missing", 7) == 7


def test_redact_hides_secrets_everywhere():
    assert redact("u=alice&p=hunter2&t=abc&s=salt&apiKey=k") == "u=alice&p=***&t=***&s=***&apiKey=***"
    assert redact("/rest/ping.view?p=secret&u=x") == "/rest/ping.view?p=***&u=x"
    assert "secret" not in repr(Params.parse("u=a&p=secret"))
