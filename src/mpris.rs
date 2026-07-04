use crate::audio::AppInfo;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// How far *before* a stream's first-seen time art may have been written and
/// still be considered that stream's art (covers art landing slightly ahead of
/// the sink-input being observed).
const PRE_WINDOW: Duration = Duration::from_secs(2);
/// How far *after* a stream's first-seen time art may arrive (browsers often
/// write media-session art a few seconds after the audio element starts).
const POST_WINDOW: Duration = Duration::from_secs(4);
/// Minimum age of an observed stream before we commit to an art decision. Gives
/// the browser time to write its art file before we look. The existing delayed
/// refresh (~2s) is the refresh that clears this gate.
pub const SETTLE: Duration = Duration::from_millis(1500);

/// Enumerate unclaimed MPRIS art files for a PID, each paired with its birth
/// time (falls back to mtime if the filesystem has no btime).
/// Firefox writes art to ~/.config/mozilla/firefox/firefox-mpris/{PID}_{counter}.png.
/// Since multiple streams share a PID, callers pass the set of paths already
/// claimed by other channels so a new slot never picks up another slot's art.
fn list_art_candidates(pid: u32, exclude: &HashSet<PathBuf>) -> Vec<(PathBuf, SystemTime)> {
    let mut out: Vec<(PathBuf, SystemTime)> = Vec::new();

    let Ok(home) = std::env::var("HOME") else {
        return out;
    };
    let mpris_dir = PathBuf::from(home).join(".config/mozilla/firefox/firefox-mpris");
    if !mpris_dir.exists() {
        return out;
    }

    let prefix = format!("{}_", pid);
    if let Ok(entries) = std::fs::read_dir(&mpris_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(&prefix) && name_str.ends_with(".png") {
                let path = entry.path();
                if exclude.contains(&path) {
                    continue;
                }
                if let Ok(meta) = entry.metadata() {
                    // Prefer birth time; fall back to mtime where btime is absent.
                    if let Ok(ts) = meta.created().or_else(|_| meta.modified()) {
                        out.push((path, ts));
                    }
                }
            }
        }
    }

    out
}

/// Result of selecting art for a stream from the candidate files.
#[derive(Debug, PartialEq)]
pub enum ArtPick {
    Match(PathBuf),
    NoArt,
}

/// Pure art-selection logic (no filesystem), so it can be unit-tested.
///
/// - `first_seen = Some(t)`: the stream's start was observed. Keep only files
///   whose btime falls within `[t - PRE_WINDOW, t + POST_WINDOW]` and pick the
///   one closest to `t`, biasing toward art written at/after `t`. No file in the
///   window → `NoArt` (we refuse to borrow an unrelated sibling's art).
/// - `first_seen = None`: the stream was already playing at cold start (true
///   start unknown). If it's the only browser stream on its PID, best-effort
///   pick the newest file; if it shares the PID with others, `NoArt` (we can't
///   disambiguate without a timing anchor).
pub fn pick_art(
    candidates: &[(PathBuf, SystemTime)],
    first_seen: Option<SystemTime>,
    same_pid_count: usize,
) -> ArtPick {
    match first_seen {
        Some(t) => {
            let lo = t.checked_sub(PRE_WINDOW);
            let hi = t.checked_add(POST_WINDOW);

            // Track the best candidate by (distance to t, prefer at/after t).
            let mut best: Option<(&PathBuf, Duration, bool)> = None;
            for (path, bt) in candidates {
                if let Some(lo) = lo {
                    if *bt < lo {
                        continue;
                    }
                }
                if let Some(hi) = hi {
                    if *bt > hi {
                        continue;
                    }
                }
                let (dist, after) = match bt.duration_since(t) {
                    Ok(d) => (d, true),              // bt >= t
                    Err(e) => (e.duration(), false), // bt <  t
                };
                let better = match &best {
                    None => true,
                    Some((_, best_dist, best_after)) => {
                        dist < *best_dist || (dist == *best_dist && after && !*best_after)
                    }
                };
                if better {
                    best = Some((path, dist, after));
                }
            }

            match best {
                Some((path, _, _)) => ArtPick::Match(path.clone()),
                None => ArtPick::NoArt,
            }
        }
        None => {
            if same_pid_count > 1 {
                return ArtPick::NoArt;
            }
            match candidates.iter().max_by_key(|(_, bt)| *bt) {
                Some((path, _)) => ArtPick::Match(path.clone()),
                None => ArtPick::NoArt,
            }
        }
    }
}

/// Check if a name is a generic placeholder that shouldn't be displayed.
pub fn is_generic_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower == "audiostream"
        || lower == "audio stream"
        || lower == "audio-src"
        || lower == "app_stream"
        || lower == "playback"
}

/// Try to claim unclaimed MPRIS art for a PID, tying the file to the stream by
/// timing (see [`pick_art`]). Returns the file path (so the caller can mark it
/// claimed) and the bytes, or `None` when nothing correlates confidently.
pub fn claim_art(
    pid: u32,
    first_seen: Option<SystemTime>,
    same_pid_count: usize,
    exclude: &HashSet<PathBuf>,
) -> Option<(PathBuf, Vec<u8>)> {
    let candidates = list_art_candidates(pid, exclude);
    match pick_art(&candidates, first_seen, same_pid_count) {
        ArtPick::Match(path) => {
            let bytes = std::fs::read(&path).ok()?;
            Some((path, bytes))
        }
        ArtPick::NoArt => None,
    }
}

/// Trailing boilerplate that browsers append to the page <title>. Stripped
/// (case-insensitively) so the header shows just the channel/stream name.
const TITLE_SUFFIXES: &[&str] = &[
    " - Watch Live on Kick",
    " - Kick",
    " - Twitch",
    " - YouTube",
];

/// Strip known site boilerplate suffixes from a page title.
/// "PaymoneyWubby Stream - Watch Live on Kick" -> "PaymoneyWubby Stream"
pub(crate) fn clean_stream_title(title: &str) -> String {
    let trimmed = title.trim();
    for suffix in TITLE_SUFFIXES {
        let Some(idx) = trimmed.len().checked_sub(suffix.len()) else {
            continue;
        };
        // A multi-byte char straddling the split point means the ASCII suffix
        // can't be there; slicing would panic (titles carry emoji), so check
        // the boundary.
        if trimmed.is_char_boundary(idx) && trimmed[idx..].eq_ignore_ascii_case(suffix) {
            return trimmed[..idx].trim().to_string();
        }
    }
    trimmed.to_string()
}

/// Read `xesam:title` from an MPRIS player's metadata over D-Bus.
async fn mpris_title(conn: &zbus::Connection, bus_name: &str) -> Option<String> {
    use std::collections::HashMap;
    use zbus::zvariant::OwnedValue;

    let props = zbus::fdo::PropertiesProxy::builder(conn)
        .destination(bus_name)
        .ok()?
        .path("/org/mpris/MediaPlayer2")
        .ok()?
        .build()
        .await
        .ok()?;

    let iface = zbus::names::InterfaceName::try_from("org.mpris.MediaPlayer2.Player").ok()?;
    let metadata: OwnedValue = props.get(iface, "Metadata").await.ok()?;
    let dict: HashMap<String, OwnedValue> = metadata.try_into().ok()?;
    String::try_from(dict.get("xesam:title")?.clone()).ok()
}

/// Recover real names for streams whose PulseAudio `media.name` is a generic
/// placeholder (e.g. "AudioStream"). Sites that don't use the W3C Media Session
/// API — Kick.com being the motivating case — leave `media.name` generic, but
/// the browser still publishes the page <title> over MPRIS. We match the MPRIS
/// player to the stream by owner PID and borrow its `xesam:title`.
///
/// Limitation: a browser process exposes a single MPRIS player reflecting its
/// active media session, so two generic streams in the same process would both
/// receive that one title. Streams with a real `media.name` (Twitch, YouTube)
/// are left untouched.
pub async fn enrich_generic_names(apps: &mut [AppInfo]) {
    if !apps
        .iter()
        .any(|a| a.pid.is_some() && is_generic_name(&a.app_name))
    {
        return;
    }

    let Ok(conn) = zbus::Connection::session().await else {
        return;
    };
    let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else {
        return;
    };
    let Ok(names) = dbus.list_names().await else {
        return;
    };

    // Map each MPRIS player bus name to the PID that owns it.
    let mut player_pids: Vec<(String, u32)> = Vec::new();
    for owned in names {
        let name = owned.to_string();
        if !name.starts_with("org.mpris.MediaPlayer2.") {
            continue;
        }
        let Ok(bus_name) = zbus::names::BusName::try_from(name.as_str()) else {
            continue;
        };
        if let Ok(pid) = dbus.get_connection_unix_process_id(bus_name).await {
            player_pids.push((name, pid));
        }
    }
    if player_pids.is_empty() {
        return;
    }

    // Names already held by a non-generic live stream, keyed by PID. A browser
    // process exposes a single MPRIS player reflecting its *active* media
    // session, so its title may belong to a sibling tab rather than the generic
    // stream we're enriching. If the title already names another live stream in
    // the same process, copying it onto this one would just duplicate that
    // sibling's identity — exactly what happens to a stream that briefly reports
    // a generic media.name after an in-browser mute/unmute recreates it. Skip
    // those; the mixer recovers such a returning stream's real identity from its
    // remembered art instead (see `mixer::update_mixer_channels`).
    let owned_names: HashSet<(u32, String)> = apps
        .iter()
        .filter_map(|a| a.pid.map(|pid| (pid, a)))
        .filter(|(_, a)| !is_generic_name(&a.app_name))
        .map(|(pid, a)| (pid, a.app_name.to_lowercase()))
        .collect();

    for app in apps.iter_mut() {
        let Some(pid) = app.pid else { continue };
        if !is_generic_name(&app.app_name) {
            continue;
        }
        for (name, owner_pid) in &player_pids {
            if *owner_pid != pid {
                continue;
            }
            if let Some(title) = mpris_title(&conn, name).await {
                let cleaned = clean_stream_title(&title);
                if !cleaned.is_empty()
                    && !is_generic_name(&cleaned)
                    && !owned_names.contains(&(pid, cleaned.to_lowercase()))
                {
                    app.app_name = cleaned;
                    break;
                }
            }
        }
    }
}

/// Schedule a delayed full refresh.
/// Waits 2 seconds for MPRIS art files and PulseAudio metadata to stabilize,
/// then re-queries everything so text and icons both update.
pub fn schedule_delayed_refresh() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        crate::audio::pulse::pulse_monitor::refresh_guarded("delayed").await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_kick_suffix() {
        assert_eq!(
            clean_stream_title("PaymoneyWubby Stream - Watch Live on Kick"),
            "PaymoneyWubby Stream"
        );
    }

    #[test]
    fn strips_case_insensitively_and_trims() {
        assert_eq!(clean_stream_title("Cool Channel - youtube"), "Cool Channel");
        assert_eq!(clean_stream_title("  Spaced Out - Twitch  "), "Spaced Out");
    }

    #[test]
    fn leaves_unrecognized_titles_untouched() {
        assert_eq!(clean_stream_title("Just A Title"), "Just A Title");
    }

    #[test]
    fn emoji_title_with_mid_char_split_point_does_not_panic() {
        // Regression: same unchecked byte-slice bug as kick.rs strip_suffix_ci —
        // len - suffix.len() landing inside a multi-byte char panicked.
        assert_eq!(clean_stream_title("🎵🎵🎵🎵"), "🎵🎵🎵🎵");
        assert_eq!(clean_stream_title("🎵 Song - YouTube"), "🎵 Song");
    }

    // --- pick_art selection logic ---

    /// A fixed base time well clear of UNIX_EPOCH so we can offset in either
    /// direction without underflowing.
    fn base() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
    }

    fn at(offset_secs: i64) -> SystemTime {
        if offset_secs >= 0 {
            base() + Duration::from_secs(offset_secs as u64)
        } else {
            base() - Duration::from_secs((-offset_secs) as u64)
        }
    }

    fn cand(name: &str, offset_secs: i64) -> (PathBuf, SystemTime) {
        (PathBuf::from(name), at(offset_secs))
    }

    #[test]
    fn observed_match_in_window() {
        let cands = vec![cand("a.png", 1)];
        assert_eq!(
            pick_art(&cands, Some(base()), 1),
            ArtPick::Match(PathBuf::from("a.png"))
        );
    }

    #[test]
    fn observed_no_match_outside_window() {
        // +10s is past POST_WINDOW (5s).
        let cands = vec![cand("a.png", 10)];
        assert_eq!(pick_art(&cands, Some(base()), 1), ArtPick::NoArt);
    }

    #[test]
    fn observed_picks_closest() {
        let cands = vec![cand("far.png", 4), cand("near.png", 1)];
        assert_eq!(
            pick_art(&cands, Some(base()), 1),
            ArtPick::Match(PathBuf::from("near.png"))
        );
    }

    #[test]
    fn observed_tiebreak_prefers_after() {
        // Equal distance (1s) on either side → prefer art written at/after start.
        let cands = vec![cand("before.png", -1), cand("after.png", 1)];
        assert_eq!(
            pick_art(&cands, Some(base()), 1),
            ArtPick::Match(PathBuf::from("after.png"))
        );
    }

    #[test]
    fn cold_start_single_stream_picks_newest() {
        let cands = vec![cand("old.png", -100), cand("new.png", -10)];
        assert_eq!(
            pick_art(&cands, None, 1),
            ArtPick::Match(PathBuf::from("new.png"))
        );
    }

    #[test]
    fn cold_start_ambiguous_pid_refuses() {
        let cands = vec![cand("a.png", -10), cand("b.png", -5)];
        assert_eq!(pick_art(&cands, None, 2), ArtPick::NoArt);
    }

    #[test]
    fn cold_start_no_files_is_no_art() {
        assert_eq!(pick_art(&[], None, 1), ArtPick::NoArt);
    }

    // Live check against the running session bus. Run with:
    //   cargo test -- --ignored --nocapture live_mpris_titles
    #[tokio::test]
    #[ignore]
    async fn live_mpris_titles() {
        let conn = zbus::Connection::session().await.unwrap();
        let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
        let names = dbus.list_names().await.unwrap();
        let mut found = 0;
        for owned in names {
            let name = owned.to_string();
            if !name.starts_with("org.mpris.MediaPlayer2.") {
                continue;
            }
            let bus = zbus::names::BusName::try_from(name.as_str()).unwrap();
            let pid = dbus.get_connection_unix_process_id(bus).await.unwrap();
            let title = mpris_title(&conn, &name).await;
            let cleaned = title.as_deref().map(clean_stream_title);
            println!("player={name} pid={pid} title={title:?} cleaned={cleaned:?}");
            found += 1;
        }
        println!("MPRIS players found: {found}");
    }
}
