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

/// What the evidence supports.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Resolution {
    /// Wear `key`, labelled `name`.
    Adopt { key: String, name: String },
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
        .or_else(|| {
            ctx.sessions.recover_recent_identity(
                pid,
                &ctx.recovery_worn,
                &state.departed_keys_for(pid, now),
                now,
            )
        });
    match recovered {
        Some(key) => {
            let name = ctx
                .sessions
                .display_name_for(pid, &key)
                .unwrap_or_else(|| fallback_name.to_string());
            Resolution::Adopt { key, name }
        }
        None => Resolution::Refuse,
    }
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
        }
    } else {
        Resolution::Defer
    }
}
