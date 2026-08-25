//! Scenario replay: drive `reconcile` through a recorded sequence of refreshes
//! and check what the deck would show after each one.
//!
//! Each `fixtures/*.json` file is one incident from the field, written as the
//! sequence of PulseAudio snapshots and session-table states that produced
//! it. The runner keeps one `IdentityState` across steps exactly as the live
//! plugin does, so the fixtures exercise the temporal rules (departure
//! windows, claim confirmation, deferred shifts) that unit tests of single
//! functions can't.
//!
//! Format (times are milliseconds from an arbitrary base):
//! ```json
//! { "name": "...",
//!   "steps": [
//!     { "at_ms": 0,
//!       "sessions": [ {"pid":1,"key":"ttv:x","title":"...","artist":"X","active_ms":0} ],
//!       "apps":     [ {"uid":10,"pid":1,"name":"X - Twitch"} ],
//!       "expect":   [ {"uid":10,"identity":"ttv:x","name":"X - Twitch"} ],
//!       "expect_wakeup": false, "expect_poll": false } ] }
//! ```
//! `sessions` carries forward when omitted. `expect` is the complete channel
//! set: a uid missing from it must not exist. `"identity": null` asserts the
//! channel is generic; an omitted `identity`/`name` is not checked.
//! `expect_wakeup`/`expect_poll` assert the effects when present.

use super::sessions::{SessionRow, SessionSnapshot};
use super::test_support::app;
use super::{reconcile, IdentityState, RefreshInput};
use serde::Deserialize;
use std::time::{Duration, SystemTime};

#[derive(Deserialize)]
struct Fixture {
    name: String,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    at_ms: u64,
    #[serde(default)]
    sessions: Option<Vec<SessionSpec>>,
    apps: Vec<AppSpec>,
    expect: Vec<Expect>,
    #[serde(default)]
    expect_wakeup: Option<bool>,
    #[serde(default)]
    expect_poll: Option<bool>,
}

#[derive(Deserialize)]
struct SessionSpec {
    pid: u32,
    key: String,
    title: String,
    #[serde(default)]
    artist: Option<String>,
    active_ms: u64,
    /// When the session last transitioned (known non-Playing) → Playing.
    #[serde(default)]
    resumed_ms: Option<u64>,
}

#[derive(Deserialize)]
struct AppSpec {
    uid: u32,
    pid: u32,
    name: String,
}

#[derive(Deserialize)]
struct Expect {
    uid: u32,
    /// Present-and-null ⇒ must be generic; absent ⇒ unchecked.
    #[serde(default, deserialize_with = "deserialize_checked")]
    identity: Option<Option<String>>,
    #[serde(default)]
    name: Option<String>,
}

/// Distinguish `"identity": null` (check: must be None) from an absent key
/// (don't check).
fn deserialize_checked<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(d).map(Some)
}

fn base() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
}

fn run(fixture: &Fixture) {
    let mut state = IdentityState::default();
    let mut channels = Vec::new();
    let mut sessions = SessionSnapshot::default();

    for (i, step) in fixture.steps.iter().enumerate() {
        let now = base() + Duration::from_millis(step.at_ms);
        let at = format!("{} step {} (t={}ms)", fixture.name, i, step.at_ms);
        if let Some(rows) = &step.sessions {
            sessions = SessionSnapshot::new(
                rows.iter()
                    .map(|r| SessionRow {
                        pid: r.pid,
                        key: r.key.clone(),
                        title: r.title.clone(),
                        artist: r.artist.clone(),
                        last_active: base() + Duration::from_millis(r.active_ms),
                        resumed_at: r.resumed_ms.map(|ms| base() + Duration::from_millis(ms)),
                    })
                    .collect(),
            );
        }
        let apps: Vec<_> = step
            .apps
            .iter()
            .map(|a| app(a.uid, Some(a.pid), &a.name))
            .collect();

        let fx = reconcile(
            &mut state,
            RefreshInput {
                prev: channels,
                apps: &apps,
                sessions: &sessions,
                now,
            },
        );
        channels = fx.channels;

        let got: Vec<String> = channels
            .iter()
            .map(|c| format!("uid={} {}={:?}", c.uid, c.identity.as_deref().unwrap_or("-"), c.app_name))
            .collect();
        let mut expected_uids: Vec<u32> = step.expect.iter().map(|e| e.uid).collect();
        expected_uids.sort();
        let mut actual_uids: Vec<u32> = channels.iter().map(|c| c.uid).collect();
        actual_uids.sort();
        assert_eq!(actual_uids, expected_uids, "{at}: channel set; got {got:?}");

        for e in &step.expect {
            let ch = channels.iter().find(|c| c.uid == e.uid).unwrap();
            if let Some(identity) = &e.identity {
                assert_eq!(
                    ch.identity.as_deref(),
                    identity.as_deref(),
                    "{at}: identity of uid={}; got {got:?}",
                    e.uid
                );
            }
            if let Some(name) = &e.name {
                assert_eq!(&ch.app_name, name, "{at}: name of uid={}; got {got:?}", e.uid);
            }
        }
        if let Some(w) = step.expect_wakeup {
            assert_eq!(fx.wake_after.is_some(), w, "{at}: wake_after");
        }
        if let Some(p) = step.expect_poll {
            assert_eq!(fx.poll_players, p, "{at}: poll_players");
        }
    }
}

#[test]
fn replay_all_fixtures() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/reconcile/fixtures");
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .expect("fixtures dir")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().map_or(false, |x| x == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures found in {dir}");
    for path in paths {
        let text = std::fs::read_to_string(&path).unwrap();
        let fixture: Fixture = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        println!("--- replay {}", fixture.name);
        run(&fixture);
    }
}
