# The NixOS module next to a real Navidrome: the service starts hardened, reads its
# credential, relays Subsonic calls and answers its own API.
{ module }:
{
  name = "navidrome-proxy";

  nodes.machine =
    { config, pkgs, ... }:
    {
      imports = [ module ];
      services.navidrome = {
        enable = true;
        settings = {
          Address = "127.0.0.1";
          Port = 4533;
          MusicFolder = "/srv/music";
          DevAutoCreateAdminPassword = "adminpw";
        };
      };
      systemd.tmpfiles.settings."10-music"."/srv/music".d = {
        user = "navidrome";
        group = "navidrome";
        mode = "2775";
      };
      environment.etc."navidrome-proxy-password".text = "adminpw";
      services.navidrome-proxy = {
        enable = true;
        group = "navidrome";
        musicDir = "/srv/music";
        navidromeUser = "admin";
        navidromePasswordFile = "/etc/navidrome-proxy-password";
        openFirewall = true;
        lyrics = false;
        ytdlpArgs = [ "--extractor-args" "youtube:player_client=visionos" ];
      };
      environment.systemPackages = [
        pkgs.curl
        pkgs.jq
        config.services.navidrome-proxy.package
      ];
    };

  testScript = ''
    machine.wait_for_unit("navidrome.service")
    machine.wait_for_open_port(4533)
    machine.wait_for_unit("navidrome-proxy.service")
    machine.wait_for_open_port(8084)

    machine.succeed("curl -sf http://127.0.0.1:8084/proxy/v1/health | jq -e '.ok == true'")
    # Relayed to Navidrome.
    machine.succeed("curl -sf 'http://127.0.0.1:8084/rest/ping.view?u=admin&p=adminpw&v=1.16.1&c=test&f=json' | jq -e '.\"subsonic-response\".status == \"ok\"'")
    # Authenticated by Navidrome, answered by the proxy.
    machine.succeed("curl -sf 'http://127.0.0.1:8084/proxy/v1/status?u=admin&p=adminpw' | jq -e '.pending.stars == 0'")
    machine.succeed("test \"$(curl -s -o /dev/null -w '%{http_code}' 'http://127.0.0.1:8084/proxy/v1/status?u=admin&p=wrong')\" = 401")
    # The import page and the Navidrome UI behind the proxy.
    machine.succeed("curl -sf http://127.0.0.1:8084/proxy/import | grep -q 'Import tracks'")
    machine.succeed("curl -sf http://127.0.0.1:8084/app/ >/dev/null")

    # State in the StateDirectory, settings from the module, secret as a credential only.
    machine.succeed("test -f /var/lib/navidrome-proxy/proxy.sqlite3")
    env = machine.succeed("systemctl show navidrome-proxy -p Environment")
    assert "YTDLP_ARGS=--extractor-args youtube:player_client=visionos" in env, env
    assert "adminpw" not in env, env
    machine.succeed("systemctl show navidrome-proxy -p User | grep -q navidrome-proxy")

    # Maintenance commands refuse to share the database with the running server.
    cli = "runuser -u navidrome-proxy -- env DATA_DIR=/var/lib/navidrome-proxy MUSIC_DIR=/srv/music navidrome-proxy"
    machine.fail(f"{cli} check-db")
    machine.succeed("systemctl stop navidrome-proxy")
    machine.succeed(f"{cli} check-db | jq -e .ok")
  '';
}
