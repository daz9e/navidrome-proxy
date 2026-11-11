{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.navidrome-proxy;
  inherit (lib) mkOption types;

  # Every runtime setting: option name -> environment variable, type, default, description.
  settings = {
    host = { env = "HOST"; type = types.str; default = "127.0.0.1"; description = "Address to listen on."; };
    port = { env = "PORT"; type = types.port; default = 8084; description = "Port to listen on."; };
    navidromeUrl = { env = "NAVIDROME_URL"; type = types.str; default = "http://127.0.0.1:4533"; description = "Base URL of the upstream Navidrome."; };
    navidromeUser = { env = "NAVIDROME_USER"; type = types.str; default = ""; description = "Optional Navidrome admin for background work (startScan needs admin). Its password goes to `navidromePasswordFile`."; };
    musicDir = { env = "MUSIC_DIR"; type = types.path; default = null; description = "The folder Navidrome scans; downloads are written here."; };
    dataDir = { env = "DATA_DIR"; type = types.path; default = "/var/lib/navidrome-proxy"; description = "SQLite state, cover cache and backups."; };
    slskdUrl = { env = "SLSKD_URL"; type = types.str; default = ""; description = "slskd base URL; with an API key it enables Soulseek quality upgrades."; };
    slskdDownloadsDir = { env = "SLSKD_DOWNLOADS_DIR"; type = types.path; default = "/var/lib/slskd/downloads"; description = "slskd's download folder (read and cleaned by upgrades)."; };
    backgroundWorkers = { env = "BACKGROUND_WORKERS"; type = types.ints.positive; default = 5; description = "Concurrent background downloads (album likes, imports, upgrades)."; };
    priorityWorkers = { env = "PRIORITY_WORKERS"; type = types.ints.positive; default = 16; description = "Concurrent play/like downloads."; };
    streamWaitSeconds = { env = "STREAM_WAIT_SECONDS"; type = types.number; default = 120; description = "How long a first play waits for its download."; };
    authCacheSeconds = { env = "AUTH_CACHE_SECONDS"; type = types.number; default = 300; description = "How long accepted credentials are cached."; };
    searchCacheSeconds = { env = "SEARCH_CACHE_SECONDS"; type = types.number; default = 120; description = "Catalog search cache TTL."; };
    searchBudgetSeconds = { env = "SEARCH_BUDGET_SECONDS"; type = types.number; default = 4; description = "Time budget for external searches per request."; };
    deezerRequestsPerSecond = { env = "DEEZER_REQUESTS_PER_SECOND"; type = types.number; default = 8; description = "Deezer API rate limit."; };
    scanDebounceSeconds = { env = "SCAN_DEBOUNCE_SECONDS"; type = types.number; default = 3; description = "Coalesce Navidrome scans after downloads."; };
    durationToleranceSeconds = { env = "DURATION_TOLERANCE_SECONDS"; type = types.number; default = 10; description = "Allowed length difference between catalog and audio source."; };
    externalSongs = { env = "EXTERNAL_SONGS"; type = types.ints.unsigned; default = 25; description = "Catalog songs per search."; };
    externalAlbums = { env = "EXTERNAL_ALBUMS"; type = types.ints.unsigned; default = 10; description = "Catalog albums per search."; };
    externalArtists = { env = "EXTERNAL_ARTISTS"; type = types.ints.unsigned; default = 5; description = "Catalog artists per search."; };
    youtubeSongs = { env = "YOUTUBE_SONGS"; type = types.ints.unsigned; default = 3; description = "YouTube-only songs per search (0 disables YouTube Music search)."; };
    completeLocalAlbums = { env = "COMPLETE_LOCAL_ALBUMS"; type = types.bool; default = true; description = "Show missing catalog tracks in local albums and artists."; };
    prefetchNextTracks = { env = "PREFETCH_NEXT_TRACKS"; type = types.ints.unsigned; default = 2; description = "Album tracks downloaded ahead of the one playing."; };
    slskdUpgrades = { env = "SLSKD_UPGRADES"; type = types.bool; default = true; description = "Replace liked tracks with verified better Soulseek copies."; };
    slskdMinPeers = { env = "SLSKD_MIN_PEERS"; type = types.ints.positive; default = 2; description = "Identical copies required before a Soulseek file is tried."; };
    lyrics = { env = "LYRICS"; type = types.bool; default = true; description = "Fetch lyrics from lrclib.net next to downloads."; };
    backupHours = { env = "BACKUP_HOURS"; type = types.number; default = 6; description = "State database backup interval (0 disables)."; };
    backupKeep = { env = "BACKUP_KEEP"; type = types.ints.positive; default = 8; description = "Backups to keep."; };
    logLevel = { env = "LOG_LEVEL"; type = types.enum [ "ERROR" "WARNING" "INFO" "DEBUG" "TRACE" ]; default = "INFO"; description = "Log level."; };
    ytdlpArgs = { env = "YTDLP_ARGS"; type = types.listOf types.str; default = [ ]; example = [ "--extractor-args" "youtube:player_client=visionos" ]; description = "Extra arguments for every yt-dlp call."; };
    ytdlpFastClient = { env = "YTDLP_FAST_CLIENT"; type = types.str; default = "visionos"; description = "YouTube player client tried first when resolving audio (faster first play); empty disables."; };
    ytdlpPath = { env = "YTDLP_PATH"; type = types.nullOr types.str; default = null; description = "yt-dlp executable (default: the one bundled with the package)."; };
    ffmpegPath = { env = "FFMPEG_PATH"; type = types.nullOr types.str; default = null; description = "ffmpeg executable."; };
    ffprobePath = { env = "FFPROBE_PATH"; type = types.nullOr types.str; default = null; description = "ffprobe executable."; };
    fpcalcPath = { env = "FPCALC_PATH"; type = types.nullOr types.str; default = null; description = "fpcalc (Chromaprint) executable."; };
  };

  toEnv =
    v:
    if builtins.isBool v then
      lib.boolToString v
    else if builtins.isList v then
      lib.concatStringsSep " " v
    else
      toString v;

  environment =
    lib.mapAttrs' (name: s: lib.nameValuePair s.env (toEnv cfg.${name})) (
      lib.filterAttrs (name: _: cfg.${name} != null) settings
    )
    // lib.optionalAttrs (cfg.navidromePasswordFile != null) { NAVIDROME_PASSWORD_FILE = "%d/navidrome-password"; }
    // lib.optionalAttrs (cfg.slskdApiKeyFile != null) { SLSKD_API_KEY_FILE = "%d/slskd-api-key"; }
    // cfg.extraEnvironment;

  defaultDataDir = cfg.dataDir == settings.dataDir.default;
in
{
  options.services.navidrome-proxy =
    {
      enable = lib.mkEnableOption "navidrome-proxy, an OpenSubsonic proxy that adds an external catalog to Navidrome";
      package = lib.mkPackageOption pkgs "navidrome-proxy" { };
      user = mkOption {
        type = types.str;
        default = "navidrome-proxy";
        description = "User the service runs as (created when left at the default).";
      };
      group = mkOption {
        type = types.str;
        default = "navidrome-proxy";
        example = "navidrome";
        description = "Group of the service; use Navidrome's group so it can read the downloads.";
      };
      openFirewall = mkOption {
        type = types.bool;
        default = false;
        description = "Open `port` in the firewall.";
      };
      navidromePasswordFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = "File with the password of `navidromeUser` (passed as a systemd credential).";
      };
      slskdApiKeyFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = "File with the slskd API key (passed as a systemd credential).";
      };
      environmentFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = "Extra `KEY=value` environment file (e.g. secrets from sops/agenix).";
      };
      extraEnvironment = mkOption {
        type = types.attrsOf types.str;
        default = { };
        description = "Additional environment variables.";
      };
    }
    // lib.mapAttrs (
      _: s:
      mkOption (
        {
          inherit (s) type description;
        }
        // lib.optionalAttrs (s.default != null || lib.hasPrefix "nullOr" (s.type.name or "")) { inherit (s) default; }
        // lib.optionalAttrs (s ? example) { inherit (s) example; }
      )
    ) settings;

  config = lib.mkIf cfg.enable {
    users.users = lib.mkIf (cfg.user == "navidrome-proxy") {
      navidrome-proxy = {
        isSystemUser = true;
        inherit (cfg) group;
        home = cfg.dataDir;
      };
    };
    users.groups = lib.mkIf (cfg.group == "navidrome-proxy") { navidrome-proxy = { }; };

    # The music folder is not created or chowned here: it belongs to the Navidrome setup.
    systemd.tmpfiles.settings."10-navidrome-proxy" = lib.mkIf (!defaultDataDir) {
      ${toString cfg.dataDir}.d = {
        inherit (cfg) user group;
        mode = "0750";
      };
    };

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ cfg.port ];

    systemd.services.navidrome-proxy = {
      description = "navidrome-proxy";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ] ++ lib.optional config.services.navidrome.enable "navidrome.service";
      inherit environment;
      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} serve";
        User = cfg.user;
        Group = cfg.group;
        Restart = "on-failure";
        RestartSec = 5;
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
        LoadCredential =
          lib.optional (cfg.navidromePasswordFile != null) "navidrome-password:${cfg.navidromePasswordFile}"
          ++ lib.optional (cfg.slskdApiKeyFile != null) "slskd-api-key:${cfg.slskdApiKeyFile}";
        StateDirectory = lib.mkIf defaultDataDir "navidrome-proxy";
        StateDirectoryMode = "0750";
        WorkingDirectory = cfg.dataDir;
        # Downloads must be readable by Navidrome.
        UMask = "0002";

        ReadWritePaths =
          [ (toString cfg.musicDir) ]
          ++ lib.optional (!defaultDataDir) (toString cfg.dataDir)
          ++ lib.optional (cfg.slskdUrl != "") (toString cfg.slskdDownloadsDir);
        ProtectSystem = "strict";
        ProtectHome = lib.mkDefault "read-only";
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectHostname = true;
        ProtectClock = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectProc = "invisible";
        NoNewPrivileges = true;
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        # yt-dlp runs a JavaScript engine (deno/V8) for YouTube's player challenges: it needs JIT.
        MemoryDenyWriteExecute = false;
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" "~@privileged" ];
      };
    };
  };
}
