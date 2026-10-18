//! "Volume Memory" keypad action.
//!
//! Hold stores the volume and mute state of every source currently on the
//! mixer — the store fires, and the key flashes, the moment the hold reaches
//! [`LONG_PRESS_MS`], while the key is still down. A tap recalls. Each button instance keeps its own
//! snapshot in its action settings, so any number of memory buttons can
//! coexist and the snapshots survive plugin and OpenDeck restarts.
//!
//! A companion "Clear Preset" action works in two steps: press it to arm
//! (it reads "Tap the button to clear it" and every stored memory button
//! turns red), then tap the memory button to erase. Pressing the clear key
//! again, or waiting [`ARM_TIMEOUT_SECS`], disarms without clearing.
//! Holding the clear key for [`CLEAR_ALL_HOLD_MS`] erases every preset on
//! the deck at once.
//!
//! Recall matches stored entries to live sources by media identity first
//! (so a Twitch tab finds its own entry even though every Firefox tab shares
//! an `app_id`), then by exact app + stream name, then by app alone (so a
//! stored "Spotify at 40%" still applies after the track changed). Sources
//! with no stored entry are left untouched; stored entries whose source is
//! gone are ignored.

use openaction::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::{
    audio, gfx,
    mixer::{self, MixerChannel},
};

/// Hold at least this long to store; anything shorter recalls. Matches the
/// grid action's long-press threshold so the two feel the same.
pub const LONG_PRESS_MS: u128 = 1000;

/// How long the key stays bright after a store, so the flash is visible but
/// doesn't lag the finger lifting.
const STORE_FLASH_MS: u64 = 160;

/// Hold the clear key this long to erase every preset. Deliberately longer
/// than a store hold so it can't happen by accident.
pub const CLEAR_ALL_HOLD_MS: u64 = 3000;

/// How long the clear key reads "all cleared" after a clear-all hold.
const CLEAR_ALL_FLASH_MS: u64 = 600;

/// An armed clear key disarms itself after this long with no memory key
/// tapped, so a forgotten press can't erase a preset minutes later.
pub const ARM_TIMEOUT_SECS: u64 = 10;

/// Default key text when no label is set: "Preset N", where N is the key's
/// position among the visible memory keys in reading order.
const DEFAULT_LABEL_PREFIX: &str = "Preset";

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct VolumeMemorySettings {
    /// Text drawn on the button; empty means [`DEFAULT_LABEL`].
    pub label: String,
    pub snapshot: Option<MemorySnapshot>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct MemorySnapshot {
    /// Unix seconds when the snapshot was taken.
    pub saved_at: u64,
    pub entries: Vec<MemoryEntry>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct MemoryEntry {
    pub app_id: String,
    pub app_name: String,
    /// Resolved media identity (e.g. "ttv:nmplol") when the stream had one.
    pub identity: Option<String>,
    pub is_device: bool,
    pub vol_percent: f32,
    pub mute: bool,
}

/// One volume/mute write produced by [`plan_recall`].
#[derive(Clone, Debug, PartialEq)]
pub struct RecallStep {
    pub app_name: String,
    pub member_uids: Vec<u32>,
    pub is_device: bool,
    pub vol_percent: f32,
    pub mute: bool,
}

/// Capture every channel, in slot order, as a snapshot.
pub fn snapshot_channels<'a>(
    channels: impl IntoIterator<Item = &'a MixerChannel>,
    saved_at: u64,
) -> MemorySnapshot {
    MemorySnapshot {
        saved_at,
        entries: channels
            .into_iter()
            .map(|ch| MemoryEntry {
                app_id: ch.app_id.clone(),
                app_name: ch.app_name.clone(),
                identity: ch.identity.clone(),
                is_device: ch.is_device,
                vol_percent: ch.vol_percent,
                mute: ch.mute,
            })
            .collect(),
    }
}

/// Decide which stored entry each live channel recalls.
///
/// Three passes, each only over channels still unmatched:
/// 1. identity equality (consumes the entry),
/// 2. `app_id` + `app_name` equality (consumes the entry),
/// 3. `app_id` alone — prefers an unconsumed entry but will reuse one, so a
///    single stored "Firefox" level applies to every open tab.
pub fn plan_recall(snapshot: &MemorySnapshot, channels: &[MixerChannel]) -> Vec<RecallStep> {
    let entries = &snapshot.entries;
    let mut used = vec![false; entries.len()];
    let mut assigned: Vec<Option<usize>> = vec![None; channels.len()];

    let same_kind = |e: &MemoryEntry, ch: &MixerChannel| e.is_device == ch.is_device;

    // Pass 1: identity.
    for (ci, ch) in channels.iter().enumerate() {
        let Some(id) = ch.identity.as_deref() else { continue };
        if let Some(ei) = entries.iter().enumerate().position(|(i, e)| {
            !used[i] && same_kind(e, ch) && e.identity.as_deref() == Some(id)
        }) {
            used[ei] = true;
            assigned[ci] = Some(ei);
        }
    }

    // Pass 2: exact app + name.
    for (ci, ch) in channels.iter().enumerate() {
        if assigned[ci].is_some() {
            continue;
        }
        if let Some(ei) = entries.iter().enumerate().position(|(i, e)| {
            !used[i] && same_kind(e, ch) && e.app_id == ch.app_id && e.app_name == ch.app_name
        }) {
            used[ei] = true;
            assigned[ci] = Some(ei);
        }
    }

    // Pass 3: app alone, unconsumed first, then any.
    for (ci, ch) in channels.iter().enumerate() {
        if assigned[ci].is_some() {
            continue;
        }
        let candidates = || {
            entries
                .iter()
                .enumerate()
                .filter(|(_, e)| same_kind(e, ch) && e.app_id == ch.app_id)
        };
        let pick = candidates()
            .find(|(i, _)| !used[*i])
            .or_else(|| candidates().next())
            .map(|(i, _)| i);
        if let Some(ei) = pick {
            used[ei] = true;
            assigned[ci] = Some(ei);
        }
    }

    channels
        .iter()
        .zip(assigned)
        .filter_map(|(ch, ei)| {
            let e = &entries[ei?];
            Some(RecallStep {
                app_name: ch.app_name.clone(),
                member_uids: ch.member_uids.clone(),
                is_device: ch.is_device,
                vol_percent: e.vol_percent,
                mute: e.mute,
            })
        })
        .collect()
}

/// A memory key currently held down.
#[derive(Clone, Copy)]
struct Press {
    started: Instant,
    /// Set once the hold timer has stored, so the release does nothing more.
    stored: bool,
}

/// Keys currently held, by instance id, so a release can tell a tap from a
/// hold and the hold timer can see whether its press is still live. Shared
/// by the memory and clear keys (instance ids never collide). Separate from
/// the grid action's press control so the two never confuse each other's
/// buttons.
static PRESS_STARTED: LazyLock<Mutex<HashMap<String, Press>>> =
    LazyLock::new(|| Mutex::const_new(HashMap::new()));

/// Record a fresh press for `instance_id` and return its start time.
async fn begin_press(instance_id: &str) -> Instant {
    let started = Instant::now();
    PRESS_STARTED
        .lock()
        .await
        .insert(instance_id.to_string(), Press { started, stored: false });
    started
}

/// Hold-timer handshake: if the press that began at `started` is still
/// down and unconsumed, consume it and return true; otherwise false.
async fn consume_hold(instance_id: &str, started: Instant) -> bool {
    let mut presses = PRESS_STARTED.lock().await;
    match presses.get_mut(instance_id) {
        Some(p) if p.started == started && !p.stored => {
            p.stored = true;
            true
        }
        _ => false,
    }
}

/// The memory button whose preset is currently "loaded": the last one that
/// stored or successfully recalled. Rendered highlighted so the deck shows
/// which preset the mixer was last set to. Instance id; `None` until a
/// button is used after startup.
static LOADED_PRESET: LazyLock<Mutex<Option<String>>> =
    LazyLock::new(|| Mutex::const_new(None));

/// Last-known settings per visible instance. The SDK keeps settings private
/// on `Instance`, and the previously loaded button must be re-rendered (to
/// drop its highlight) without an event carrying its settings.
static SETTINGS_CACHE: LazyLock<Mutex<HashMap<String, VolumeMemorySettings>>> =
    LazyLock::new(|| Mutex::const_new(HashMap::new()));

/// True while the mixer has no sources at all. Memory buttons are blanked
/// then — there is nothing to store or recall — and redrawn when audio
/// returns. Flipped only by [`sync_with_mixer`].
static DARK: AtomicBool = AtomicBool::new(false);

/// True while a clear key is armed: the next memory key tapped is erased
/// instead of recalled or stored.
static CLEAR_ARMED: AtomicBool = AtomicBool::new(false);

/// Bumped on every arm/disarm so a stale auto-disarm timer from an earlier
/// arming can't cancel a later one.
static ARM_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Re-evaluate the blank/drawn state against the mixer. Called after every
/// refresh; only a transition touches the deck, so steady state costs one
/// lock and no image writes.
pub async fn sync_with_mixer() {
    let empty = mixer::MIXER_CHANNELS.lock().await.is_empty();
    if DARK.swap(empty, Ordering::AcqRel) == empty {
        return;
    }
    if empty && CLEAR_ARMED.swap(false, Ordering::AcqRel) {
        // Nothing left to clear; the blanked keys can't show the armed state.
        ARM_GENERATION.fetch_add(1, Ordering::AcqRel);
        println!("Clear preset: disarmed (mixer went empty)");
    }
    refresh_memory_buttons().await;
    refresh_clear_buttons().await;
}

/// Redraw every memory button from its cached settings.
async fn refresh_memory_buttons() {
    let empty = DARK.load(Ordering::Acquire);
    let cache = SETTINGS_CACHE.lock().await.clone();
    for inst in visible_instances(VolumeMemoryAction::UUID).await {
        match cache.get(&inst.instance_id) {
            Some(settings) => render(&inst, settings).await,
            // Appeared before we cached anything: blank it if that's the
            // state, otherwise leave it for its own will_appear render.
            None if empty => blank(&inst).await,
            None => {}
        }
    }
}

/// Flip the armed flag and redraw every key that shows it. Returns the new
/// generation, or `None` when already in that state (nothing redrawn).
async fn apply_armed(armed: bool) -> Option<u64> {
    if CLEAR_ARMED.swap(armed, Ordering::AcqRel) == armed {
        return None;
    }
    let generation = ARM_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    refresh_memory_buttons().await;
    refresh_clear_buttons().await;
    Some(generation)
}

/// Arm or disarm the clear step. Arming starts the auto-disarm timer, which
/// only fires if nothing re-armed or disarmed in the meantime.
async fn set_armed(armed: bool) {
    let Some(generation) = apply_armed(armed).await else { return };
    if armed {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(ARM_TIMEOUT_SECS)).await;
            if ARM_GENERATION.load(Ordering::Acquire) == generation && CLEAR_ARMED.load(Ordering::Acquire) {
                println!("Clear preset: disarmed after {ARM_TIMEOUT_SECS}s with no memory key tapped");
                apply_armed(false).await;
            }
        });
    }
}

async fn blank(instance: &Instance) {
    let _ = instance
        .set_image(Some(gfx::TRANSPARENT_ICON.as_str()), None)
        .await;
}

async fn remember_settings(instance: &Instance, settings: &VolumeMemorySettings) {
    SETTINGS_CACHE
        .lock()
        .await
        .insert(instance.instance_id.clone(), settings.clone());
}

/// Mark `instance` as the loaded preset and un-highlight whichever button
/// held that status before.
async fn mark_loaded(instance: &Instance, settings: &VolumeMemorySettings) {
    let previous = LOADED_PRESET
        .lock()
        .await
        .replace(instance.instance_id.clone());
    render(instance, settings).await;

    if let Some(prev_id) = previous.filter(|id| id != &instance.instance_id) {
        let prev_settings = SETTINGS_CACHE.lock().await.get(&prev_id).cloned();
        if let (Some(prev), Some(prev_settings)) = (get_instance(prev_id).await, prev_settings) {
            render(&prev, &prev_settings).await;
        }
    }
}

/// The key's text: its custom label, or "Preset N" by position.
async fn display_label(instance: &Instance, settings: &VolumeMemorySettings) -> String {
    let label = settings.label.trim();
    if !label.is_empty() {
        return label.to_string();
    }
    format!("{DEFAULT_LABEL_PREFIX} {}", preset_ordinal(instance).await)
}

/// 1-based position of `instance` among the visible memory keys, counted
/// top to bottom then left to right (reading order), per device. Keys with
/// no coordinates (inside a multi action) come last, in id order.
async fn preset_ordinal(instance: &Instance) -> usize {
    let mut keys: Vec<_> = visible_instances(VolumeMemoryAction::UUID)
        .await
        .into_iter()
        .map(|i| (preset_sort_key(&i), i.instance_id.clone()))
        .collect();
    if !keys.iter().any(|(_, id)| id == &instance.instance_id) {
        // Not listed yet (first render during will_appear on some hosts).
        keys.push((preset_sort_key(instance), instance.instance_id.clone()));
    }
    keys.sort();
    keys.iter()
        .position(|(_, id)| id == &instance.instance_id)
        .map_or(1, |p| p + 1)
}

fn preset_sort_key(instance: &Instance) -> (String, bool, u8, u8) {
    match instance.coordinates {
        Some(c) => (instance.device_id.clone(), false, c.row, c.column),
        None => (instance.device_id.clone(), true, 0, 0),
    }
}

async fn render_clear(instance: &Instance) {
    if DARK.load(Ordering::Acquire) {
        blank(instance).await;
        return;
    }
    let face = if CLEAR_ARMED.load(Ordering::Acquire) {
        gfx::ClearFace::Armed
    } else {
        gfx::ClearFace::Idle
    };
    match gfx::get_clear_button_data_uri(face) {
        Ok(uri) => {
            let _ = instance.set_image(Some(uri), None).await;
        }
        Err(e) => eprintln!("Failed to render clear-preset button image: {e}"),
    }
}

/// Redraw every clear button so they all show the same armed/idle state.
async fn refresh_clear_buttons() {
    for inst in visible_instances(ClearPresetAction::UUID).await {
        render_clear(&inst).await;
    }
}

/// A clear key was pressed: arm if idle, disarm if already armed. Arming
/// is refused (with an alert) when no memory key on the deck holds a preset.
async fn toggle_armed(button: &Instance) -> OpenActionResult<()> {
    if CLEAR_ARMED.load(Ordering::Acquire) {
        println!("Clear preset: disarmed (clear key pressed again)");
        set_armed(false).await;
        return Ok(());
    }
    let any_stored = SETTINGS_CACHE.lock().await.values().any(|s| s.snapshot.is_some());
    if !any_stored {
        println!("Clear preset: no memory key holds a preset");
        let _ = button.show_alert().await;
        return Ok(());
    }
    println!("Clear preset: armed, waiting for a memory key tap");
    set_armed(true).await;
    Ok(())
}

/// Erase every visible memory key's stored volumes (labels kept), drop the
/// loaded highlight, disarm, and show "all cleared" on `button` briefly.
async fn clear_all_presets(button: &Instance) {
    let cache = SETTINGS_CACHE.lock().await.clone();
    let mut cleared = 0usize;
    for inst in visible_instances(VolumeMemoryAction::UUID).await {
        let Some(settings) = cache.get(&inst.instance_id) else { continue };
        if settings.snapshot.is_none() {
            continue;
        }
        let updated = VolumeMemorySettings {
            label: settings.label.clone(),
            snapshot: None,
        };
        match inst.set_settings(&updated).await {
            Ok(()) => {
                remember_settings(&inst, &updated).await;
                cleared += 1;
            }
            Err(e) => eprintln!("Clear preset: failed to clear {}: {e}", inst.instance_id),
        }
    }
    *LOADED_PRESET.lock().await = None;
    println!("Clear preset: hold cleared all {cleared} stored preset(s)");

    if let Ok(uri) = gfx::get_clear_button_data_uri(gfx::ClearFace::AllCleared) {
        let _ = button.set_image(Some(uri), None).await;
    }
    // Redraw the memory keys now (empty, unarmed) but leave the clear key
    // on its flash face until the pause is over.
    if apply_armed(false).await.is_none() {
        refresh_memory_buttons().await;
    }
    tokio::time::sleep(Duration::from_millis(CLEAR_ALL_FLASH_MS)).await;
    refresh_clear_buttons().await;
    if cleared > 0 {
        let _ = button.show_ok().await;
    } else {
        let _ = button.show_alert().await;
    }
}

/// Clear-key hold timer: after [`CLEAR_ALL_HOLD_MS`] with the key still
/// down, clear every preset and consume the press so the release doesn't
/// also arm.
fn arm_clear_all_timer(instance_id: String, started: Instant) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(CLEAR_ALL_HOLD_MS)).await;
        if !consume_hold(&instance_id, started).await {
            return;
        }
        if let Some(button) = get_instance(instance_id).await {
            clear_all_presets(&button).await;
        }
    });
}

/// Erase this memory key's stored volumes (keeping its label), drop its
/// highlight if it was the loaded preset, and disarm.
async fn clear_preset(instance: &Instance, settings: &VolumeMemorySettings) -> OpenActionResult<()> {
    if settings.snapshot.is_none() {
        println!(
            "Clear preset: '{}' ({}) is already empty",
            display_label(instance, settings).await,
            instance.instance_id
        );
        set_armed(false).await;
        let _ = instance.show_alert().await;
        return Ok(());
    }
    let updated = VolumeMemorySettings {
        label: settings.label.clone(),
        snapshot: None,
    };
    instance.set_settings(&updated).await?;
    remember_settings(instance, &updated).await;
    {
        let mut loaded = LOADED_PRESET.lock().await;
        if loaded.as_deref() == Some(instance.instance_id.as_str()) {
            *loaded = None;
        }
    }
    println!(
        "Clear preset: erased preset '{}' ({})",
        display_label(instance, &updated).await,
        instance.instance_id
    );
    // Disarming redraws every memory key, this one included, from the cache.
    set_armed(false).await;
    let _ = instance.show_ok().await;
    Ok(())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Live channels in slot order, cloned out from under the mixer lock.
async fn ordered_channels() -> Vec<MixerChannel> {
    let channels = mixer::MIXER_CHANNELS.lock().await;
    let mut ordered: Vec<(u8, MixerChannel)> =
        channels.iter().map(|(slot, ch)| (*slot, ch.clone())).collect();
    ordered.sort_by_key(|(slot, _)| *slot);
    ordered.into_iter().map(|(_, ch)| ch).collect()
}

async fn render(instance: &Instance, settings: &VolumeMemorySettings) {
    if DARK.load(Ordering::Acquire) {
        blank(instance).await;
        return;
    }
    let label = display_label(instance, settings).await;
    let stored = settings.snapshot.as_ref().map(|s| s.entries.len());
    let loaded = LOADED_PRESET.lock().await.as_deref() == Some(instance.instance_id.as_str());
    let armed = CLEAR_ARMED.load(Ordering::Acquire);
    match gfx::get_memory_button_data_uri(&label, stored, loaded, armed, false) {
        Ok(uri) => {
            let _ = instance.set_image(Some(uri), None).await;
        }
        Err(e) => eprintln!("Failed to render memory button image: {e}"),
    }
}

/// Briefly draw the key bright ("saved") so a hold gets instant feedback
/// the moment it has stored, then hand back to the normal render.
async fn flash_saved(instance: &Instance, settings: &VolumeMemorySettings) {
    let label = display_label(instance, settings).await;
    let stored = settings.snapshot.as_ref().map(|s| s.entries.len());
    if let Ok(uri) = gfx::get_memory_button_data_uri(&label, stored, true, false, true) {
        let _ = instance.set_image(Some(uri), None).await;
        tokio::time::sleep(Duration::from_millis(STORE_FLASH_MS)).await;
    }
}

async fn store(instance: &Instance, settings: &VolumeMemorySettings) -> OpenActionResult<()> {
    let channels = ordered_channels().await;
    let snapshot = snapshot_channels(&channels, now_unix_secs());
    if snapshot.entries.is_empty() {
        println!("Volume memory {}: nothing to store (no sources)", instance.instance_id);
        let _ = instance.show_alert().await;
        return Ok(());
    }

    println!(
        "Volume memory {}: stored {} sources: {}",
        instance.instance_id,
        snapshot.entries.len(),
        snapshot
            .entries
            .iter()
            .map(|e| format!("{}={}%{}", e.app_name, e.vol_percent.round(), if e.mute { " (muted)" } else { "" }))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let updated = VolumeMemorySettings {
        label: settings.label.clone(),
        snapshot: Some(snapshot),
    };
    instance.set_settings(&updated).await?;
    remember_settings(instance, &updated).await;
    flash_saved(instance, &updated).await;
    mark_loaded(instance, &updated).await;
    let _ = instance.show_ok().await;
    Ok(())
}

/// Hold timer: once the key has been down for [`LONG_PRESS_MS`], store
/// right away (still under the finger) and flag the press so the release
/// is a no-op. Does nothing if the key was released, or re-pressed, first.
fn arm_hold_timer(instance_id: String, started: Instant, settings: VolumeMemorySettings) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(LONG_PRESS_MS as u64)).await;
        if !consume_hold(&instance_id, started).await {
            return;
        }
        let Some(instance) = get_instance(instance_id).await else { return };
        if let Err(e) = store(&instance, &settings).await {
            eprintln!("Volume memory {}: store on hold failed: {e}", instance.instance_id);
        }
    });
}

/// Write every step to PulseAudio. `Err` means no audio system at all;
/// `Ok(n)` is how many individual writes failed.
fn apply_steps(steps: &[RecallStep]) -> Result<usize, String> {
    let mut audio_system = audio::create().map_err(|e| e.to_string())?;
    let mut failures = 0usize;
    for step in steps {
        for uid in &step.member_uids {
            if let Err(e) = audio_system.set_volume(*uid, step.vol_percent, step.is_device) {
                failures += 1;
                println!("Warning: Failed to set volume for {}: {}", step.app_name, e);
            }
            if let Err(e) = audio_system.mute_volume(*uid, step.mute, step.is_device) {
                failures += 1;
                println!("Warning: Failed to set mute for {}: {}", step.app_name, e);
            }
        }
    }
    Ok(failures)
}

async fn recall(instance: &Instance, settings: &VolumeMemorySettings) -> OpenActionResult<()> {
    let Some(snapshot) = settings.snapshot.as_ref() else {
        println!("Volume memory {}: recall on an empty button", instance.instance_id);
        let _ = instance.show_alert().await;
        return Ok(());
    };

    let channels = ordered_channels().await;
    let steps = plan_recall(snapshot, &channels);
    if steps.is_empty() {
        println!(
            "Volume memory {}: none of the {} stored sources are running",
            instance.instance_id,
            snapshot.entries.len()
        );
        let _ = instance.show_alert().await;
        return Ok(());
    }

    // The PulseAudio handle is !Send, so every write happens here, before
    // the next await, and only the failure count crosses it.
    let failures = match apply_steps(&steps) {
        Ok(n) => n,
        Err(e) => {
            println!("Warning: audio system unavailable: {}", e);
            let _ = instance.show_alert().await;
            return Ok(());
        }
    };

    println!(
        "Volume memory {}: recalled {} of {} stored sources ({} live): {}",
        instance.instance_id,
        steps.len(),
        snapshot.entries.len(),
        channels.len(),
        steps
            .iter()
            .map(|s| format!("{}={}%{}", s.app_name, s.vol_percent.round(), if s.mute { " (muted)" } else { "" }))
            .collect::<Vec<_>>()
            .join(", ")
    );

    mark_loaded(instance, settings).await;
    if failures == 0 {
        let _ = instance.show_ok().await;
    } else {
        let _ = instance.show_alert().await;
    }
    Ok(())
}

pub struct VolumeMemoryAction;

#[async_trait]
impl Action for VolumeMemoryAction {
    const UUID: ActionUuid = "com.victormarin.volume-controller.memory";
    type Settings = VolumeMemorySettings;

    async fn will_appear(&self, instance: &Instance, settings: &Self::Settings) -> OpenActionResult<()> {
        remember_settings(instance, settings).await;
        render(instance, settings).await;
        // A new key can shift the "Preset N" numbering of the others.
        refresh_memory_buttons().await;
        Ok(())
    }

    async fn will_disappear(&self, instance: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        PRESS_STARTED.lock().await.remove(&instance.instance_id);
        SETTINGS_CACHE.lock().await.remove(&instance.instance_id);
        refresh_memory_buttons().await;
        Ok(())
    }

    /// Label edits (and "clear") arrive from the property inspector here.
    async fn did_receive_settings(&self, instance: &Instance, settings: &Self::Settings) -> OpenActionResult<()> {
        remember_settings(instance, settings).await;
        if settings.snapshot.is_none() {
            // A cleared preset can't be the loaded one.
            let mut loaded = LOADED_PRESET.lock().await;
            if loaded.as_deref() == Some(instance.instance_id.as_str()) {
                *loaded = None;
            }
        }
        render(instance, settings).await;
        Ok(())
    }

    async fn key_down(&self, instance: &Instance, settings: &Self::Settings) -> OpenActionResult<()> {
        let started = begin_press(&instance.instance_id).await;
        if !CLEAR_ARMED.load(Ordering::Acquire) {
            arm_hold_timer(instance.instance_id.clone(), started, settings.clone());
        }
        Ok(())
    }

    async fn key_up(&self, instance: &Instance, settings: &Self::Settings) -> OpenActionResult<()> {
        let press = PRESS_STARTED.lock().await.remove(&instance.instance_id);

        if CLEAR_ARMED.load(Ordering::Acquire) {
            // Armed by the clear key: this tap (or hold) picks what to erase.
            return clear_preset(instance, settings).await;
        }
        match press {
            // The hold timer already stored and flashed while the key was down.
            Some(Press { stored: true, .. }) => Ok(()),
            // Timer hasn't fired yet but the hold was long enough (it lost a
            // race with the release): store now rather than drop the intent.
            Some(Press { started, .. }) if started.elapsed().as_millis() >= LONG_PRESS_MS => {
                store(instance, settings).await
            }
            _ => recall(instance, settings).await,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ClearPresetSettings {}

pub struct ClearPresetAction;

#[async_trait]
impl Action for ClearPresetAction {
    const UUID: ActionUuid = "com.victormarin.volume-controller.memory-clear";
    type Settings = ClearPresetSettings;

    async fn will_appear(&self, instance: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        render_clear(instance).await;
        Ok(())
    }

    async fn will_disappear(&self, instance: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        PRESS_STARTED.lock().await.remove(&instance.instance_id);
        // Only disarm when the last clear key leaves; the others still show it.
        if visible_instances(ClearPresetAction::UUID).await.is_empty() {
            set_armed(false).await;
        }
        Ok(())
    }

    async fn key_down(&self, instance: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        let started = begin_press(&instance.instance_id).await;
        arm_clear_all_timer(instance.instance_id.clone(), started);
        Ok(())
    }

    /// A short press arms/disarms on release; a hold was already handled
    /// (and consumed) by the clear-all timer.
    async fn key_up(&self, instance: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        let press = PRESS_STARTED.lock().await.remove(&instance.instance_id);
        match press {
            Some(Press { stored: true, .. }) => Ok(()),
            Some(Press { started, .. }) if started.elapsed().as_millis() >= CLEAR_ALL_HOLD_MS as u128 => {
                // Timer lost the race with the release; honour the hold.
                clear_all_presets(instance).await;
                Ok(())
            }
            _ => toggle_armed(instance).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::test_support::bare_channel;

    fn ch(app_id: &str, app_name: &str, identity: Option<&str>, uid: u32) -> MixerChannel {
        let mut c = bare_channel(app_name, None, false);
        c.app_id = app_id.to_string();
        c.identity = identity.map(str::to_string);
        c.uid = uid;
        c.member_uids = vec![uid];
        c
    }

    fn entry(app_id: &str, app_name: &str, identity: Option<&str>, vol: f32, mute: bool) -> MemoryEntry {
        MemoryEntry {
            app_id: app_id.into(),
            app_name: app_name.into(),
            identity: identity.map(str::to_string),
            is_device: false,
            vol_percent: vol,
            mute,
        }
    }

    fn snap(entries: Vec<MemoryEntry>) -> MemorySnapshot {
        MemorySnapshot { saved_at: 0, entries }
    }

    #[test]
    fn snapshot_captures_every_channel_in_order() {
        let mut a = ch("Spotify", "Song A", None, 1);
        a.vol_percent = 40.0;
        let mut b = ch("Firefox", "Nmplol - Twitch", Some("ttv:nmplol"), 2);
        b.mute = true;
        b.vol_percent = 75.0;
        let s = snapshot_channels([&a, &b], 123);
        assert_eq!(s.saved_at, 123);
        assert_eq!(
            s.entries,
            vec![
                entry("Spotify", "Song A", None, 40.0, false),
                entry("Firefox", "Nmplol - Twitch", Some("ttv:nmplol"), 75.0, true),
            ]
        );
    }

    #[test]
    fn identity_beats_app_name_for_browser_tabs() {
        // Two Firefox tabs swapped names since the snapshot (Firefox's media.name
        // lie); the identity still finds the right level for each.
        let s = snap(vec![
            entry("Firefox", "Nmplol - Twitch", Some("ttv:nmplol"), 20.0, false),
            entry("Firefox", "Xqc - Twitch", Some("ttv:xqc"), 80.0, false),
        ]);
        let live = vec![
            ch("Firefox", "Xqc - Twitch", Some("ttv:nmplol"), 10),
            ch("Firefox", "Nmplol - Twitch", Some("ttv:xqc"), 11),
        ];
        let steps = plan_recall(&s, &live);
        assert_eq!(steps.len(), 2);
        assert_eq!((steps[0].member_uids[0], steps[0].vol_percent), (10, 20.0));
        assert_eq!((steps[1].member_uids[0], steps[1].vol_percent), (11, 80.0));
    }

    #[test]
    fn app_level_fallback_applies_after_track_change() {
        // Spotify's app_name is the track title; a different track still
        // gets the stored Spotify level. A never-stored app is untouched.
        let s = snap(vec![entry("Spotify", "Song A", None, 40.0, true)]);
        let live = vec![
            ch("Spotify", "Song B", None, 5),
            ch("Discord", "Discord", None, 6),
        ];
        let steps = plan_recall(&s, &live);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].member_uids, vec![5]);
        assert_eq!(steps[0].vol_percent, 40.0);
        assert!(steps[0].mute);
    }

    #[test]
    fn exact_name_match_wins_before_app_fallback() {
        let s = snap(vec![
            entry("Firefox", "Tab A", None, 30.0, false),
            entry("Firefox", "Tab B", None, 60.0, false),
        ]);
        // Live order is reversed; names must still pair up exactly.
        let live = vec![ch("Firefox", "Tab B", None, 2), ch("Firefox", "Tab A", None, 1)];
        let steps = plan_recall(&s, &live);
        assert_eq!(steps[0].vol_percent, 60.0);
        assert_eq!(steps[1].vol_percent, 30.0);
    }

    #[test]
    fn one_stored_browser_level_covers_every_open_tab() {
        let s = snap(vec![entry("Firefox", "Old tab", None, 25.0, false)]);
        let live = vec![
            ch("Firefox", "New tab 1", Some("ttv:a"), 1),
            ch("Firefox", "New tab 2", Some("ttv:b"), 2),
        ];
        let steps = plan_recall(&s, &live);
        assert_eq!(steps.len(), 2);
        assert!(steps.iter().all(|s| s.vol_percent == 25.0));
    }

    #[test]
    fn device_entries_never_match_streams_of_the_same_name() {
        let mut dev_entry = entry("System Audio", "System Audio", None, 50.0, false);
        dev_entry.is_device = true;
        let s = snap(vec![dev_entry]);
        let stream = ch("System Audio", "System Audio", None, 1);
        assert!(plan_recall(&s, &[stream]).is_empty());

        let mut dev = ch("System Audio", "System Audio", None, 9);
        dev.is_device = true;
        let steps = plan_recall(&s, &[dev]);
        assert_eq!(steps.len(), 1);
        assert!(steps[0].is_device);
    }

    #[test]
    fn collapsed_app_recalls_every_member_stream() {
        let s = snap(vec![entry("Game", "Game", None, 70.0, false)]);
        let mut game = ch("Game", "Game", None, 1);
        game.member_uids = vec![1, 2, 3];
        let steps = plan_recall(&s, &[game]);
        assert_eq!(steps[0].member_uids, vec![1, 2, 3]);
    }

    #[test]
    fn settings_tolerate_missing_fields_and_old_snapshots() {
        let s: VolumeMemorySettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, VolumeMemorySettings::default());
        let s: VolumeMemorySettings =
            serde_json::from_str(r#"{"label":"Game","snapshot":{"entries":[{"app_id":"Spotify"}]}}"#)
                .unwrap();
        assert_eq!(s.label, "Game");
        assert_eq!(s.snapshot.unwrap().entries[0].app_id, "Spotify");
    }
}
