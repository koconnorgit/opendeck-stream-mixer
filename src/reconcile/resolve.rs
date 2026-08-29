//! The identity resolver: every rule that turns evidence into an identity
//! lives here, once. `reconcile_existing` and `build_new` only decide which
//! question to ask.
//!
//! Two questions exist:
//!  - `recover_untrusted`: the stream's reported name is worthless (generic,
//!    or a title another live stream also carries). Which identity, if any,
//!    does the evidence point to?
//!  - `claim_reported`: the stream's reported name resolves to an identity.
//!    May it wear it now, or must the claim ride a confirmation window?
//!
//! Both refuse rather than guess. A WRONG identity is permanent (a channel
//! wearing one dismisses its own generic name as noise), while a refused one
//! degrades to a transient generic label that keeps retrying every refresh.

use super::{Ctx, IdentityState};
use std::time::SystemTime;

/// What the evidence supports.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Resolution {
    /// Wear `key`, labelled `name`. `floor` is set when the identity came
    /// from the departure trail alone (the time of the departure that seeded
    /// it): such an adoption is an inference, not a signal, and rides a short
    /// provisional window during which a fresh session signal newer than the
    /// floor may correct it. Session-backed and name-backed adoptions carry
    /// no floor — the browser itself named them.
    Adopt {
        key: String,
        name: String,
        floor: Option<SystemTime>,
    },
    /// The claim is plausible but ambiguous for now; the caller should ask
    /// for a confirming refresh after the claim window.
    Defer,
    /// Nothing trustworthy; the caller should ask for a player poll.
    Refuse,
}

/// Re-identify a stream whose reported name can't be trusted.
///
/// Departures first: the identity that died in this very refresh (a mute's
/// same-batch replacement) or a uniquely fresh/sole departure names a stream
/// that really vanished — while a session from a just-closed tab can still
/// look "freshest" (ghost). The session tiers then cover unmutes that
/// re-signal and the already-active-tab case. `fallback_name` labels the
/// channel when the session table has no canonical name for the identity.
pub(super) fn recover_untrusted(
    pid: Option<u32>,
    fallback_name: &str,
    ctx: &Ctx<'_>,
    state: &IdentityState,
) -> Resolution {
    let Some(pid) = pid else {
        return Resolution::Refuse;
    };
    let now = ctx.now;
    let recovered = state
        .recover_departed_identity(pid, &ctx.recovery_worn, ctx.sessions, now)
        .map(|(key, departed_at)| (key, Some(departed_at)))
        .or_else(|| {
            // Only departures nobody wears count as unclaimed.
            let unclaimed: std::collections::HashMap<String, SystemTime> = state
                .departed_for(pid, now)
                .into_iter()
                .filter(|(k, _)| !ctx.recovery_worn.contains(k))
                .collect();
            ctx.sessions
                .recover_recent_identity(pid, &ctx.recovery_worn, &unclaimed, now)
                .map(|key| (key, None))
        });
    match recovered {
        Some((key, floor)) => {
            let name = ctx
                .sessions
                .display_name_for(pid, &key)
                .unwrap_or_else(|| fallback_name.to_string());
            Resolution::Adopt { key, name, floor }
        }
        None => Resolution::Refuse,
    }
}

/// Whether a fresh session signal contradicts a provisional (departure-trail)
/// identity: the freshest unworn session that signaled after `floor` — the
/// departure the trail pick was seeded from — names what the browser actually
/// started playing (the streamer-switch race: that signal lands ~0.5-1s after
/// the sink-input it describes). A DEPARTED session signaling now is excluded:
/// that is Firefox's main-controller echo after an unmute (the mizkif case),
/// not the new stream's own voice.
pub(super) fn provisional_correction(
    pid: Option<u32>,
    floor: SystemTime,
    ctx: &Ctx<'_>,
    state: &IdentityState,
) -> Option<(String, String)> {
    let pid = pid?;
    let now = ctx.now;
    let key = ctx
        .sessions
        .fresh_signal_since(pid, &ctx.recovery_worn, floor, now)?;
    if state.departed_for(pid, now).contains_key(&key) {
        return None;
    }
    let name = ctx
        .sessions
        .display_name_for(pid, &key)
        .unwrap_or_else(|| key.clone());
    Some((key, name))
}

/// A stream reports a name that resolves to `key`. Adopt it — unless that
/// identity JUST departed, which is ambiguous: it may be the muted tab
/// genuinely returning, or the OTHER returning stream wearing that title
/// (Firefox names new sink-inputs after the active session — this is how
/// muting twitch+kick and unmuting in reverse order swapped both slots).
/// Such claims ride the global (pid, identity) confirmation window, which
/// survives the stream's own uid churn: a genuine return keeps the title and
/// confirms; the lie gets renamed by later media.name updates and resets it.
pub(super) fn claim_reported(
    pid: Option<u32>,
    key: String,
    raw: &str,
    ctx: &Ctx<'_>,
    state: &mut IdentityState,
) -> Resolution {
    let now = ctx.now;
    if !state.identity_departed_recently(pid, &key, now) {
        return Resolution::Adopt {
            key,
            name: raw.to_string(),
            floor: None,
        };
    }
    let confirmed = pid
        .map(|pid| state.deferred_claim_confirmed(pid, &key, raw, now))
        .unwrap_or(false);
    if confirmed {
        println!("Deferred adoption confirmed for pid={pid:?}: {key}");
        Resolution::Adopt {
            key,
            name: raw.to_string(),
            floor: None,
        }
    } else {
        Resolution::Defer
    }
}
