"""One recording, many ids.

The same song reaches the proxy as a Deezer track, a YouTube Music video, a
SoundCloud upload, and sometimes under a slightly different title ("Юпи - Порезы
на руках" vs "Порезы на руках"). Without a shared identity each of those would be
downloaded again and listed twice.

Every downloaded track has one canonical id (the id it was first requested as).
Other ids of the same recording become *aliases* of it:

* exact aliases – the source a download actually came from (a Deezer track fetched
  from YouTube video V makes ``yt:V`` an alias), the ISRC, and ids linked by the
  catalog reconciler;
* recording keys – "primary artist | base title" plus duration (±3 s), which finds
  the same song across catalogs without any shared id.

An audio fingerprint is the final arbiter when metadata disagrees (see
:mod:`audio`), but that needs both files and is only used for upgrades.
"""

from __future__ import annotations

from typing import Any

from . import ids, matching
from .db import Store, Track

DURATION_SLACK = 3.0


def recording_key(artists: list[str] | str | None, title: str | None) -> str | None:
    if isinstance(artists, str):
        artists = matching.split_artists(artists)
    lead = matching.normalize((artists or [""])[0])
    base = matching.base_title(title)
    if not lead or not base:
        return None
    return f"{lead}|{base}"


class Identity:
    def __init__(self, store: Store):
        self.store = store

    def canonical(self, track_id: str) -> str:
        """Follow the alias chain (at most a few hops) to the canonical id."""
        seen = {track_id}
        current = track_id
        for _ in range(5):
            nxt = self.store.resolve_alias(current)
            if nxt is None or nxt in seen:
                return current
            seen.add(nxt)
            current = nxt
        return current

    def canonical_many(self, track_ids: list[str]) -> dict[str, str]:
        mapping = self.store.resolve_aliases(track_ids)
        return {t: (self.canonical(mapping[t]) if t in mapping else t) for t in track_ids}

    def link(self, alias: str, canonical: str, how: str) -> bool:
        target = self.canonical(canonical)
        if self.canonical(alias) == target:
            return False
        return self.store.add_alias(alias, target, how)

    def register(self, track: Track) -> None:
        """Record the identities of a (downloaded) track."""
        meta = track.meta
        if meta.get("isrc"):
            self.store.add_alias(f"isrc:{str(meta['isrc']).upper()}", track.id, "isrc")
        key = recording_key(track.artists, track.title)
        if key:
            duration = float(track.audio.get("duration") or track.duration or 0)
            self.store.add_recording(key, track.id, duration)
        source_id = self._source_id(track)
        if source_id and source_id != track.id:
            self.store.add_alias(source_id, track.id, f"source:{track.source}")

    @staticmethod
    def _source_id(track: Track) -> str | None:
        ref = track.source_ref or ""
        if track.source == "youtube" and ids.valid_youtube_id(ref):
            return ids.youtube(ref)
        if track.source == "soundcloud" and ref.isdigit():
            return ids.soundcloud(ref)
        return None

    def find_existing(
        self, meta: dict[str, Any], exclude: str | None = None, usable: bool = True
    ) -> Track | None:
        """A known track that is the same recording as ``meta`` (ISRC, then key+duration)."""
        candidates: list[str] = []
        if meta.get("isrc"):
            hit = self.store.resolve_alias(f"isrc:{str(meta['isrc']).upper()}")
            if hit:
                candidates.append(hit)
        key = recording_key(meta.get("artists") or meta.get("artist"), meta.get("title"))
        duration = float(meta.get("duration") or 0)
        if key:
            for track_id, known in self.store.recordings(key):
                if duration and known and abs(known - duration) > DURATION_SLACK:
                    continue
                candidates.append(track_id)
        for track_id in dict.fromkeys(candidates):
            canonical = self.canonical(track_id)
            if canonical == exclude:
                continue
            track = self.store.get_track(canonical)
            if track is None:
                continue
            if not usable or track.is_ready:
                return track
        return None
