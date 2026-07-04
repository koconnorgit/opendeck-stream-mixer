# Design: MPRIS subscription + URL-keyed identity/art

Status: phase 1 (shadow mode) implemented and soaking since 2026-07-04 —
`src/media_session.rs`, grep the plugin log for `MPRIS-WATCH` / `SHADOW`.
Twitch art decided: session art primary, preview CDN fallback. Replaces the
file-timing art machinery in `mpris.rs`/`mixer.rs` with an event-driven
media-session watcher and deterministic, URL-keyed art resolution.

## Why (verified facts this design rests on)

Two live D-Bus experiments on 2026-07-04 (short poll + 10-minute capture:
298 polls, 353 `PropertiesChanged` signals, ~30 sessions across YouTube
watch/Shorts, Twitch, Kick — see memory note `mpris-xesam-url-ground-truth`):

1. **`xesam:url` carries the actual tab URL** of the browser's active media
   session — `youtube.com/watch?v=<id>`, `youtube.com/shorts/<id>`,
   `twitch.tv/<login>`, `kick.com/<slug>` — even for sites without the Media
   Session API (Firefox falls back to page title + URL). Ground-truth
   identity, no browser extension needed.
2. **`mpris:artUrl` appears only as a transient burst** (1–3 signals, then
   the key is dropped, sometimes within ~450ms). Steady-state reads miss it;
   only a signal subscription sees it. When present, the (artUrl ↔ xesam:url)
   pairing was 100% consistent across all sessions.
3. **Firefox keeps exactly ONE art file on disk at a time** and deletes it on
   the next session change. Directory-scavenging + timing correlation (the
   current `claim_art`/`pick_art` design) therefore reads a source that
   structurally cannot answer "whose art is this?" during any churn. Art
   bytes must be copied at signal time.
4. **One MPRIS player per browser process**, reflecting only the *active*
   session. Two uncorked sink-inputs, one metadata source. Join key between
   metadata and `media.name` differs by site: YouTube/Reddit `media.name` ==
   `xesam:title`; Twitch `media.name` is `"<Channel> - Twitch"` which matches
   the URL slug / `xesam:artist`, NOT `xesam:title` (the stream title).
5. Sink-input `media.name` still lies (active-session title inherited by new
   sink-inputs) and flaps (`"AudioStream"`); the watcher improves detection
   but does not remove the need for collision recovery.

## Goals / non-goals

Goals:

- Correct, consistent channel icons for YouTube / Twitch / Kick without
  timing windows, file claiming, or freeze-locks.
- Delete the fragile machinery: `first_seen`, `SETTLE`, `PRE_WINDOW`,
  `POST_WINDOW`, `pick_art`, `list_art_candidates`, `claim_art`,
  `claimed_paths`, cold-start ambiguity rules, `art_resolved`.
- Make the mixer update logic testable (pure `reconcile` — deferred backlog
  item #1 falls out of this design).

Non-goals:

- Per-tab ground truth for *inactive* sessions (needs a browser extension;
  explicitly out of scope).
- Changing the user-decided collision-recovery semantics (auto-restore over
  safe, 2026-06-26) or the window-name override policy.

## Architecture

```
                   D-Bus session bus
                        │
             ┌──────────▼───────────┐   NameOwnerChanged (players come/go)
             │  media_session.rs    │   PropertiesChanged (Metadata bursts)
             │  watcher task        │
             └──────────┬───────────┘
       on signal: parse url/title/artist/artUrl,
       read art file bytes IMMEDIATELY, debounce ~250ms
                        │
             ┌──────────▼───────────┐
             │  SESSIONS table      │  (pid, MediaIdentity) → SessionRecord
             │  (replaces           │  url, title, artist, art bytes,
             │   ART_MEMORY)        │  playback status, last_active
             └──────────┬───────────┘
                        │ lookup by (pid, identity) or (pid, title)
   PulseAudio events    │
        │      ┌────────▼───────────┐      ┌────────────────────┐
        └──────►  mixer reconcile   ◄──────┤  art_resolve.rs    │
               │  (pure function)   │      │  identity → bytes  │
               └────────┬───────────┘      │  1. session art    │
                        │                  │  2. site fetcher   │
               update_stream_deck_buttons  │     (yt/twitch/    │
                                           │      kick, cached) │
                                           └────────────────────┘
```

## New module: `media_session.rs`

A single long-lived tokio task started next to `start_pulse_monitoring()`.

- **Startup:** `ListNames`, take every `org.mpris.MediaPlayer2.*`, resolve
  owner PID (`GetConnectionUnixProcessID`), read current `Metadata` +
  `PlaybackStatus` once (cold start becomes a property read — no ambiguity).
- **Watch:** `NameOwnerChanged` (players appear/disappear → add/remove
  per-player `PropertiesChanged` streams; zbus `PropertiesProxy`
  `receive_properties_changed()` per player, merged with `StreamExt`).
- **Per signal:** extract `xesam:url`, `xesam:title`, `xesam:artist`,
  `mpris:artUrl`, `PlaybackStatus`.
  - Bursts: 3 signals in ~2ms, some dicts partial (missing `mpris:length`
    etc.). Coalesce per player for ~250ms and merge fields; a field absent in
    a later signal of the same burst does not erase a value seen earlier in
    the burst (artUrl arrives in the middle and is dropped at the end).
  - If `artUrl` present: **read the file bytes now** (it is the only file on
    disk and will be deleted on the next session change). Tolerate a missing
    file (lost race) — the site fetcher covers it.
  - Derive `MediaIdentity` from the URL (below). Homepage/transitional URLs
    (`kick.com/`, `twitch.tv/`, empty path) → ignore the event.
- **Store** into `SESSIONS`; if anything user-visible changed (new identity,
  art arrived), nudge the existing refresh channel (expose a
  `request_refresh()` in `pulse_monitor` that sends on `REFRESH_CHANNEL`).
- **Player exit:** keep its sessions with a `dormant_since` stamp; prune
  after a TTL (~5 min — generous vs today's 30s `ART_MEMORY_TTL` because
  entries are keyed by exact identity and can't be mis-applied).

```rust
enum MediaIdentity {
    YouTube { video_id: String },          // watch?v=<id> | /shorts/<id>
    Twitch  { login: String },             // twitch.tv/<login>
    Kick    { slug: String },              // kick.com/<slug>
    Web     { host: String, title: String } // anything else (Reddit, …)
}

struct SessionRecord {
    identity: MediaIdentity,
    url: String,
    title: String,            // xesam:title as published
    artist: Option<String>,   // xesam:artist (Twitch channel display name)
    art: Option<Arc<Vec<u8>>>,// bytes captured at signal time (newest wins)
    playback: PlaybackStatus,
    last_active: SystemTime,  // last signal naming this identity
    dormant_since: Option<SystemTime>, // set when player vanishes
}

static SESSIONS: LazyLock<std::sync::Mutex<HashMap<(u32, IdentityKey), SessionRecord>>>;
```

`IdentityKey` is the canonical string form (`yt:<id>`, `ttv:<login>`,
`kick:<slug>`, `web:<host>:<title-lower>`), also used as the art-cache key.

## Art resolution: `art_resolve.rs`

`resolve(identity) -> ArtState` with a per-identity cache generalizing the
`kick.rs` `AVATARS` state machine (`Fetching` / `Ready(bytes)` /
`Failed(Instant)` + `FAILURE_BACKOFF`), fetched via the existing curl-binary
pattern (Cloudflare constraint stays):

| Identity | Primary source | Fallback |
|---|---|---|
| YouTube  | session art bytes (browser ground truth) | `https://i.ytimg.com/vi/<id>/mqdefault.jpg` (no auth) |
| Twitch   | session art bytes | avatar via decapi / Helix (needs decision) or `static-cdn.jtvnw.net/previews-ttv/live_user_<login>-320x180.jpg` (no auth, stream screenshot) |
| Kick     | existing `kick.rs` avatar fetch by slug (unchanged, keyed off URL slug when available, title parse otherwise) | — |
| Web      | session art bytes | none (themed icon path) |

Notes:
- Session art always wins when present — it is exactly what the site
  published for that content.
- YouTube thumbnails and Twitch previews are 16:9; center-crop square before
  handing to `get_app_icon_uri` so the LCD tile isn't letterboxed.
- Every fetch is `spawn`ed; completion nudges `request_refresh()` (same
  contract as `kick::spawn_fetch` today).

## Matching sink-inputs to identities

Per refresh, for each `AppInfo` with a PID, in order:

1. **Self-identifying `media.name`** — parse the name itself:
   `"<X> - Twitch"` → `Twitch{login: lc(X)}`; Kick tab title → `Kick{slug}`
   (existing `kick_handle`); else exact-title lookup in `SESSIONS` on this
   PID (`xesam:title == cleaned media.name` → that record's identity —
   covers YouTube and Web). This works even for sessions that were learned
   minutes ago and are no longer active.
2. **Generic or colliding name** (the lie): keep today's `title_collides`
   detection, but recovery consults `SESSIONS` instead of `ART_MEMORY`:
   dormant-or-inactive identities on the PID not currently worn by a live
   sibling; adopt only if unique, else leave generic. The watcher adds one
   new signal: if a brand-new sink-input's name equals the *currently
   active* session's title while another live stream already wears it, the
   lie is observed, not inferred.
3. **No match** → no identity; themed-icon path (wm_class / window_icon /
   icon_name) as today.

The user-facing display name comes from the identity when we have one
(`artist` for Twitch, `title` for YouTube, slug handle for Kick — reusing
`kick::display_name` semantics), else the current name pipeline unchanged.

## Mixer changes

`MixerChannel` diff:

```rust
// removed: mpris_art_data, mpris_art_path, first_seen, art_resolved, locked
// added:
pub identity: Option<IdentityKey>, // matched media identity, if any
pub art: Option<Arc<Vec<u8>>>,     // resolved art actually rendered
pub name_locked: bool,             // name-only lock (what `locked` should have been)
```

- Art is recomputed by lookup every refresh (cheap: table hit or cache hit)
  — safe because it is keyed, so there is nothing to "freeze". The
  `uses_default_icon` late-window icon upgrade loop stays as-is for
  non-browser apps.
- `name_locked` keeps only the name-stability behavior for browser channels
  (don't churn the label on every Shorts navigation once set), and is reset
  on a non-colliding name shift exactly as today.
- Deletions: `resolve_art`, `apply_art` (folded into reconcile),
  `remember_channel_art`, `remembered_art_for`,
  `recover_identity_from_memory` (replaced by SESSIONS-based recovery),
  `count_pids`, `ART_MEMORY`; in `mpris.rs`: `list_art_candidates`,
  `pick_art`, `claim_art`, `PRE_WINDOW`, `POST_WINDOW`, `SETTLE`.
  `enrich_generic_names` is subsumed by the watcher (title *and* URL, fresher).
- **Extract pure `reconcile`** (deferred backlog #1):

```rust
fn reconcile(
    prev: Option<&MixerChannel>,
    app: &AppInfo,
    sessions: &SessionView,     // read-only snapshot of SESSIONS for app.pid
    art: &dyn Fn(&IdentityKey) -> Option<Arc<Vec<u8>>>, // cache lookup only
    now: SystemTime,
) -> MixerChannel
```

  No global statics, no filesystem, no network → unit-testable, including
  the collision-recovery cases that currently only have partial coverage.

## Phasing (each phase shippable + revertible)

1. **Shadow mode.** Add `media_session.rs` + `SESSIONS`, log
   `identity/art-would-be` next to what the current pipeline chose. No
   behavior change. Soak on the real desktop; compare against the known
   failure cases (Shorts churn, Twitch mute/unmute restore, Kick flap).
2. **Art switchover.** Icons come from identity lookup + `art_resolve`;
   names/locks untouched. Delete the file-timing machinery (it has no other
   consumer). Regression bar: Twitch in-browser mute/unmute still restores
   its avatar; Kick avatar appears while `media.name` flaps; Shorts show the
   right thumbnail per short.
3. **Identity switchover.** Names + collision recovery via `SESSIONS`;
   remove `ART_MEMORY` and `enrich_generic_names`; `locked` → `name_locked`.
4. **Reconcile extraction + tests** for the whole matrix (new stream, lie,
   collision, recovery, device, multi-sink, kick-pending).

## Risks / open questions

- **Twitch avatar source** — DECIDED (2026-07-04): session art primary,
  preview CDN (`static-cdn.jtvnw.net/previews-ttv/live_user_<login>-320x180.jpg`,
  no auth) as fallback. Revisit real avatars (decapi/Helix) only if this
  proves unsatisfying.
- **Chromium**: exposes `org.mpris.MediaPlayer2.chromium.instance<pid>` with
  the same single-active-session shape; `media.name` there is usually
  generic ("Playback"), so the watcher *improves* Chromium support for free,
  but matching leans harder on step 2. Not a target; don't regress it.
- **zbus stream fan-in**: one `PropertiesProxy` stream per player must be
  added/removed dynamically; use `futures::stream::SelectAll` or a small
  per-player spawned task feeding one mpsc channel (simpler to supervise).
  Cargo.lock has zbus 5.14, whose `receive_properties_changed()` streams
  support this directly.
- **Same title, two tabs, one PID** stays irreducible when both are
  inactive-session (unchanged from today; collision rules keep the
  user-chosen trade-off).
- Watcher must be panic-contained like everything else post-684ebf4 (spawn
  with a supervisor that logs + restarts with backoff).
