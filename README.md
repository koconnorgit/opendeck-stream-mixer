# OA Volume Controller Plugin

> **Fork notice:** This is a fork of [mdvictor/opendeck-volume-controller](https://github.com/mdvictor/opendeck-volume-controller) by Victor Marin (MIT licensed). It adds encoder/dial support for Stream Deck + devices and a 200×100 encoder LCD layout with a 96×96 app icon on the left and the app name, a horizontal volume bar, and a percent readout on the right. The original button-grid functionality is unchanged.

A per-application volume control plugin for Stream Deck using OpenDeck on Linux.

![Showcase](./img/readme.png)

## Overview

Take full control of your sound experience with fine-tuned per-app volume management! This plugin integrates with PulseAudio to provide a visual mixer interface directly on your Stream Deck, allowing you to control individual application volumes with dedicated buttons.

## Features

- **Per-Application Volume Control**: Adjust volume levels for each running audio application independently
- **Visual Volume Bars**: Real-time graphical representation of volume levels on your Stream Deck
- **Mute Toggle**: Quickly mute/unmute applications with a single button press
- **System Mixer Support**: Optional system-wide mixer control
- **Auto-Detection**: Automatically discovers and tracks running audio applications
- **App Icons**: Displays application icons for easy identification
- **Kick.com streamer avatars** *(fork addition)*: Kick streams don't publish media-session art, so they'd otherwise show the generic browser icon. The plugin recovers the channel slug from the tab title and fetches the streamer's avatar from Kick's public API (requires `curl`; see Prerequisites)
- **Real-time Updates**: Monitors PulseAudio events and updates the interface dynamically
- **Ignore apps**: Exclude specific apps from showing in the volume controller
- **Encoder/Dial Support** *(fork addition)*: Assign the action to an encoder dial on Stream Deck + / Stream Deck + XL. Rotate to adjust volume, press to mute, and see the app name, icon, and a live volume bar on the encoder's LCD zone.
- **Touch-to-Mute** *(fork addition)*: Tap the encoder LCD to toggle mute, or long-press to dismiss the stream — requires a touch-enabled OpenDeck build and a [patched `openaction` fork](#patched-openaction-dependency).
- **Volume Memory** *(fork addition)*: Map any number of keys as volume presets. Hold one to store every source's current volume and mute, tap it to set them all back. See [Volume memory buttons](#volume-memory-buttons-fork-addition).

## Installation

### From source (Linux x86_64)

Prerequisites:

- Rust toolchain (`rustup` / `cargo`)
- PulseAudio, or PipeWire with the PulseAudio compatibility layer
- OpenDeck installed and running
- `curl` on `PATH` — used to fetch Kick.com streamer avatars (Kick sits behind Cloudflare, which rejects pure-Rust TLS clients; everything else still works without it)
- A sans-serif Bold font on the system for the encoder LCD title rendering — the plugin looks for Noto Sans Bold or DejaVu Sans Bold at the usual Linux font paths and silently skips the title if neither is present
- Network access to GitHub on the first build — it pulls a patched `openaction` fork (see below)

### Patched `openaction` dependency

This plugin depends on a **patched fork of the [`openaction`](https://crates.io/crates/openaction) crate**, not the version published on crates.io. `Cargo.toml` pins it via:

```toml
[patch.crates-io]
openaction = { git = "https://github.com/koconnorgit/rust", branch = "main" }
```

The fork ([**koconnorgit/rust**](https://github.com/koconnorgit/rust)) adds a `touchTap` inbound event so the plugin can react to taps on the encoder LCD touchscreen; the upstream crate has no such event and silently drops it. `cargo build` fetches the fork automatically — no manual step — but the fork's `main` branch must remain available on GitHub for builds to resolve. See that repo's README for the full rationale.

> The LCD **touch-to-mute** behaviour additionally needs an OpenDeck build that emits `touchTap` (e.g. one with Stream Deck + touchscreen support). On stock OpenDeck the touch event is never sent, so touch is simply inert — pressing the dial still mutes.

Build and install:

```sh
git clone https://github.com/koconnorgit/opendeck-volume-controller.git
cd opendeck-volume-controller
cargo build --release
```

If you already have the upstream plugin installed through OpenDeck's plugin store, you can replace just the binary:

```sh
cp target/release/oa-volume-controller \
  ~/.config/opendeck/plugins/com.victormarin.volume-controller.sdPlugin/oa-volume-controller-x86_64-unknown-linux-gnu
```

For a fresh install without the upstream plugin, also copy the plugin assets:

```sh
PLUGIN_DIR=~/.config/opendeck/plugins/com.victormarin.volume-controller.sdPlugin
mkdir -p "$PLUGIN_DIR"
cp manifest.json pi.html pi-memory.html pi-memory-clear.html LICENSE "$PLUGIN_DIR/"
cp -r img "$PLUGIN_DIR/"
cp target/release/oa-volume-controller "$PLUGIN_DIR/oa-volume-controller-x86_64-unknown-linux-gnu"
```

Reload the plugin without restarting OpenDeck:

```sh
opendeck --reload-plugin com.victormarin.volume-controller.sdPlugin
```

Or restart OpenDeck fully to pick up the new binary.

## Usage

### Button-grid layout (original)

Drag the `Volume Control Auto Grid` action across the SD grid. This was tested and developed with the SD3x5 in mind, so at least one full column (3 actions per column) is needed to show one volume mixer, where the first action button is the mixer icon together with the mute/unmute button, the second is Vol+ and the remaining button is Vol-.

After setting your grid, switch profiles and return to your volume controller profile to kick things off.

Pressing the volume app icon will mute it.
Long pressing the volume app icon will set it as ignored and remove that specific volume bar from the device. To revert this action click on any volume controller grid cell in the opendeck UI and remove it from the list of ignored apps.

### Encoder/dial layout *(fork addition)*

Assign the action directly to an encoder dial on a Stream Deck + or Stream Deck + XL. Each encoder gets its own per-app mixer:

- **Rotate** the dial to adjust volume up or down
- **Press** the dial to toggle mute
- **Tap** the LCD to toggle mute, or **long-press** it to dismiss the stream — same as a long press on the keypad icon (requires a touch-enabled OpenDeck build; see [Patched `openaction` dependency](#patched-openaction-dependency))
- The LCD zone shows the 96×96 app icon on the left, and on the right: the app name on top (scrolls horizontally if long), a horizontal volume bar, and a percent readout below; muting dims the whole zone

### Volume memory buttons *(fork addition)*

Drag the `Volume Memory` action onto any key. Each key is its own preset, so add as many as you like (one per game, one for streaming, one for "everything quiet", …).

- **Hold** the key for 1 second to store the current volume and mute state of every source on the mixer. The key flashes bright the instant the hold is long enough, while your finger is still on it, so you know the preset is saved. It then turns green and shows how many sources it holds.
- **Tap** the key to recall: every stored source that is currently running is set back to its stored volume and mute. Sources that weren't stored are left alone; stored sources that aren't running are skipped. The key flashes a checkmark on success or a warning when there was nothing to store, nothing to recall, or a PulseAudio write failed.
- The key you last stored or recalled is drawn with a filled green background and reads `loaded`, so you can tell at a glance which preset the mixer was last set to. Using another memory key moves the highlight; clearing a preset drops it. The highlight is not persisted across plugin restarts.
- Each key shows a speaker icon over its name. Unlabelled keys are named `Preset 1`, `Preset 2`, … by their position on the deck (reading order: top row left to right, then the next row), not by when they were added, so the numbering stays tidy when keys are moved or removed.
- The property inspector lets you give the key a label (shown in place of `Preset N`), lists what is stored, and can clear the preset.
- To clear a preset from the deck, add the `Clear Preset` action to a key. Press it once to arm: it turns red and reads `Tap the button to clear it`, and every memory key holding a preset is washed in red. Tap the memory key you want to erase; its stored volumes go and its label stays. Press the clear key again, or wait 10 seconds, to cancel. **Hold** the clear key for 3 seconds to erase every preset on the deck at once; it flashes `ALL cleared`.

Recall matches stored entries to live sources by media identity first (a Twitch/YouTube/Kick tab finds its own entry even though every browser tab shares an app name), then by exact app + stream name, then by app alone — so a stored "Spotify at 40%" still applies after the track changes, and a single stored browser level applies to every open tab. Presets live in the key's action settings, so they survive plugin reloads and OpenDeck restarts.

## ToDo (from upstream)

- ~~Support for dials~~ *(done in this fork)*
- Support for mini devices (2×3 grid cells)
- Manual setup for specifying which specific app would live on what column

Contributions are welcome!

## Credits

Original plugin by **Victor Marin** — [mdvictor/opendeck-volume-controller](https://github.com/mdvictor/opendeck-volume-controller), MIT licensed. This fork preserves the original license and history.
