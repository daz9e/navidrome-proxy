"""User intents that wait for a track to become local, then land in Navidrome.

Navidrome owns likes, scrobbles and playlists. For an external track the proxy
keeps the intent (per user) until the download is indexed, then replays it with
that user's credentials and forgets it. Credentials are never persisted: an
intent whose user has not been seen since a restart waits for their next request.
"""

from __future__ import annotations

import asyncio
import logging
import time

from . import ids, matching
from .db import Store, Track
from .identity import Identity
from .navidrome import Navidrome, UpstreamError, UpstreamUnavailable
from .subsonic.params import Params
from .tasks import Background

log = logging.getLogger(__name__)


class Intents:
    def __init__(self, store: Store, identity: Identity, navidrome: Navidrome, min_interval: float = 20.0):
        self.store = store
        self.identity = identity
        self.navidrome = navidrome
        self.min_interval = min_interval
        self._last_flush: dict[str, float] = {}
        self._locks: dict[str, asyncio.Lock] = {}
        self.background = Background()

    def _local_id(self, target: str) -> str | None:
        track = self.store.get_track(self.identity.canonical(target))
        if track is None or not track.is_ready:
            return None
        return track.local_id

    def maybe_flush(self, username: str, force: bool = False) -> None:
        """Schedule a flush for ``username`` (rate limited) if they have pending work."""
        now = time.monotonic()
        if not force and now - self._last_flush.get(username, 0) < self.min_interval:
            return
        if username not in self.store.users_with_pending_work():
            self._last_flush[username] = now
            return
        self._last_flush[username] = now
        self.background.spawn(self.flush(username))

    async def on_track_mapped(self, track: Track) -> None:
        """A download got its Navidrome id: replay every user's pending intents."""
        for username in self.store.users_with_pending_work():
            await self.flush(username)

    async def flush(self, username: str) -> dict[str, int]:
        creds = self.navidrome.credentials_for(username)
        done = {"stars": 0, "unstars": 0, "scrobbles": 0, "playlist": 0}
        if creds is None:
            return done
        lock = self._locks.setdefault(username, asyncio.Lock())
        async with lock:
            try:
                done["unstars"] = await self._unstars(username, creds)
                done["stars"] = await self._stars(username, creds)
                done["scrobbles"] = await self._scrobbles(username, creds)
                done["playlist"] = await self._playlists(username, creds)
            except UpstreamUnavailable as exc:
                log.info("intent flush for %s postponed: %s", username, exc)
        if any(done.values()):
            log.info("synced intents of %s to Navidrome: %s", username, done)
        return done

    async def _unstars(self, username: str, creds: Params) -> int:
        local_ids = self.store.pending_unstars(username)
        if not local_ids:
            return 0
        try:
            await self.navidrome.call_many("unstar", creds, {"id": local_ids})
        except UpstreamError as exc:
            log.warning("unstar of %d songs for %s rejected: %s", len(local_ids), username, exc)
            return 0
        self.store.remove_pending_unstars(username, local_ids)
        return len(local_ids)

    async def _stars(self, username: str, creds: Params) -> int:
        synced = 0
        songs: list[tuple[str, str]] = []
        albums: list[tuple[str, str]] = []
        artists: list[tuple[str, str]] = []
        for _, target, _ in self.store.pending_stars(username):
            vid = ids.parse(target)
            if vid is None:
                self.store.remove_pending_star(username, target)
                continue
            if vid.is_track:
                local_id = self._local_id(target)
                if local_id:
                    songs.append((target, local_id))
            elif vid.kind == "album":
                local_album = await self._local_album(vid.key, creds)
                if local_album:
                    albums.append((target, local_album))
            elif vid.kind == "artist":
                local_artist = await self._local_artist(vid.key, creds)
                if local_artist:
                    artists.append((target, local_artist))
        for key, pairs in (("id", songs), ("albumId", albums), ("artistId", artists)):
            if not pairs:
                continue
            try:
                await self.navidrome.call_many("star", creds, {key: [local for _, local in pairs]})
            except UpstreamError as exc:
                log.warning("star of %s for %s rejected: %s", pairs, username, exc)
                continue
            for target, _ in pairs:
                self.store.remove_pending_star(username, target)
                synced += 1
        return synced

    async def _local_album(self, deezer_album_id: str, creds: Params) -> str | None:
        """The Navidrome album holding any downloaded track of a Deezer album."""
        for track in self.store.tracks_in_album(deezer_album_id):
            track = self.store.get_track(self.identity.canonical(track.id)) or track
            if track.local_id and track.is_ready:
                try:
                    root = await self.navidrome.call("getSong", creds, id=track.local_id, timeout=10)
                except UpstreamError:
                    continue
                album_id = (root.get("song") or {}).get("albumId")
                if album_id:
                    return str(album_id)
        return None

    async def _local_artist(self, deezer_artist_id: str, creds: Params) -> str | None:
        track = self.store.ready_track_by_artist_id(deezer_artist_id)
        if track is None or not track.artists:
            return None
        name = track.artists[0]
        try:
            root = await self.navidrome.call(
                "search3", creds, query=name, artistCount="10", albumCount="0", songCount="0", timeout=10
            )
        except UpstreamError:
            return None
        for artist in (root.get("searchResult3") or {}).get("artist") or []:
            if matching.normalize(artist.get("name")) == matching.normalize(name):
                return str(artist["id"])
        return None

    async def _scrobbles(self, username: str, creds: Params) -> int:
        sent = 0
        for track_id, when, submission in self.store.pending_scrobbles(username):
            local_id = self._local_id(track_id)
            if not local_id:
                if time.time() - when / 1000 > 30 * 86400:
                    self.store.remove_pending_scrobble(username, track_id, when, submission)
                continue
            try:
                await self.navidrome.call(
                    "scrobble",
                    creds,
                    id=local_id,
                    time=str(when),
                    submission="true" if submission else "false",
                )
            except UpstreamError as exc:
                log.warning("scrobble of %s for %s rejected: %s", track_id, username, exc)
            self.store.remove_pending_scrobble(username, track_id, when, submission)
            sent += 1
        return sent

    async def _playlists(self, username: str, creds: Params) -> int:
        added = 0
        for playlist_id, track_id in self.store.pending_playlist_adds(username):
            local_id = self._local_id(track_id)
            if not local_id:
                continue
            try:
                await self.navidrome.call(
                    "updatePlaylist", creds, playlistId=playlist_id, songIdToAdd=local_id
                )
            except UpstreamError as exc:
                log.warning("playlist %s add of %s for %s rejected: %s", playlist_id, track_id, username, exc)
            self.store.remove_pending_playlist_add(username, playlist_id, track_id)
            added += 1
        return added
