# Stream Mixer

A per-application volume mixer for Stream Deck on Linux, running on [OpenDeck](https://github.com/nekename/OpenDeck): keypad grids, Stream Deck + dials with live LCD art, touch-to-mute, and volume memory presets.

> Stream Mixer grew out of [mdvictor/opendeck-volume-controller](https://github.com/mdvictor/opendeck-volume-controller) by **Victor Marin** (MIT). It began as a fork in March 2026 and has since become its own plugin with its own identity; see [Origins and credits](#origins-and-credits).

![Showcase](./img/readme.png)

## Overview

Stream Mixer talks to PulseAudio (or PipeWire's PulseAudio layer) and turns your Stream Deck into a live per-app mixer: every playing application gets its own channel with its icon, name, volume bar, mute, and dismiss, on keypad columns or on Stream Deck + dials. Browser tabs playing Twitch, YouTube, or Kick are identified individually, with the streamer's avatar or the video's thumbnail as the channel art. Volume memory keys store and recall whole mixer states.

## Features

- **Per-Application Volume Control**: Adjust volume levels for each running audio application independently
- **Visual Volume Bars**: Real-time graphical representation of volume levels on your Stream Deck
- **Mute Toggle**: Quickly mute/unmute applications with a single button press
- **System Mixer Support**: Optional system-wide mixer control
- **Auto-Detection**: Automatically discovers and tracks running audio applications
- **App Icons**: Displays application icons for easy identification
- **Kick.com streamer avatars**: Kick streams don't publish media-session art, so they'd otherwise show the generic browser icon. The plugin recovers the channel slug from the tab title and fetches the streamer's avatar from Kick's public API (requires `curl`; see Prerequisites)
- **Real-time Updates**: Monitors PulseAudio events and updates the interface dynamically
- **Ignore apps**: Exclude specific apps from showing in the volume controller
- **Encoder/Dial Support**: Assign the action to an encoder dial on Stream Deck + / Stream Deck + XL. Rotate to adjust volume, press to mute, and see the app name, icon, and a live volume bar on the encoder's LCD zone.
- **Touch-to-Mute**: Tap the encoder LCD to toggle mute, or long-press to dismiss the stream — requires a touch-enabled OpenDeck build and a [patched `openaction` fork](#patched-openaction-dependency).
- **Volume Memory**: Map any number of keys as volume presets. Hold one to store every source's current volume and mute, tap it to set them all back. See [Volume memory buttons](#volume-memory-buttons).

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
git clone https://github.com/koconnorgit/opendeck-stream-mixer.git
cd opendeck-stream-mixer
cargo build --release
```

Install the plugin into OpenDeck's plugin directory:

```sh
PLUGIN_DIR=~/.config/opendeck/plugins/io.github.koconnorgit.stream-mixer.sdPlugin
mkdir -p "$PLUGIN_DIR"
cp manifest.json pi.html pi-memory.html pi-memory-clear.html LICENSE "$PLUGIN_DIR/"
cp -r img "$PLUGIN_DIR/"
cp target/release/stream-mixer "$PLUGIN_DIR/stream-mixer-x86_64-unknown-linux-gnu"
```

Reload the plugin without restarting OpenDeck:

```sh
opendeck --reload-plugin io.github.koconnorgit.stream-mixer.sdPlugin
```

Or restart OpenDeck fully to pick up the new binary.

### From a release

Download `io.github.koconnorgit.stream-mixer.zip` from the [latest release](https://github.com/koconnorgit/opendeck-stream-mixer/releases), extract it into `~/.config/opendeck/plugins/`, and restart OpenDeck. The zip ships native binaries for `x86_64` and `aarch64` Linux.

### Migrating from the Volume Controller fork (1.x)

Versions up to 1.10.0 shipped under the original plugin's identity (`com.victormarin.volume-controller`). Stream Mixer 2.0.0 has its own plugin and action UUIDs, so OpenDeck sees it as a new plugin:

- Install Stream Mixer alongside, then remove or disable the old `com.victormarin.volume-controller.sdPlugin` directory so the two don't both drive your deck.
- Keys and dials on your profiles still point at the old action UUIDs. Re-add them with the Stream Mixer actions (`Mixer Channel`, `Volume Memory`, `Clear Preset`).
- Volume memory presets live in each key's settings, so they do not carry over; store them again.
- The ignored-apps list is plugin-global and does not carry over either.

## Usage

### Button-grid layout

Drag the `Mixer Channel` action across the SD grid. This was tested and developed with the SD3x5 in mind, so at least one full column (3 actions per column) is needed to show one volume mixer, where the first action button is the mixer icon together with the mute/unmute button, the second is Vol+ and the remaining button is Vol-.

After setting your grid, switch profiles and return to your volume controller profile to kick things off.

Pressing the volume app icon will mute it.
Long pressing the volume app icon will set it as ignored and remove that specific volume bar from the device. To revert this action click on any volume controller grid cell in the opendeck UI and remove it from the list of ignored apps.

### Encoder/dial layout

Assign the action directly to an encoder dial on a Stream Deck + or Stream Deck + XL. Each encoder gets its own per-app mixer:

- **Rotate** the dial to adjust volume up or down
- **Press** the dial to toggle mute
- **Tap** the LCD to toggle mute, or **long-press** it to dismiss the stream — same as a long press on the keypad icon (requires a touch-enabled OpenDeck build; see [Patched `openaction` dependency](#patched-openaction-dependency))
- The LCD zone shows the 96×96 app icon on the left, and on the right: the app name on top (scrolls horizontally if long), a horizontal volume bar, and a percent readout below; muting dims the whole zone

### Volume memory buttons

Drag the `Volume Memory` action onto any key. Each key is its own preset, so add as many as you like (one per game, one for streaming, one for "everything quiet", …).

- **Hold** the key for 1 second to store the current volume and mute state of every source on the mixer. The key flashes bright the instant the hold is long enough, while your finger is still on it, so you know the preset is saved. It then turns green and shows how many sources it holds.
- **Tap** the key to recall: every stored source that is currently running is set back to its stored volume and mute. Sources that weren't stored are left alone; stored sources that aren't running are skipped. The key flashes a checkmark on success or a warning when there was nothing to store, nothing to recall, or a PulseAudio write failed.
- The key you last stored or recalled is drawn with a filled green background and reads `loaded`, so you can tell at a glance which preset the mixer is set to. The highlight only lasts while the mixer still matches the preset: change the volume or mute of any source the preset covers, whether from the deck, the app itself, or the system mixer, and the key goes back to reading `N sources`. Using another memory key moves the highlight; clearing a preset drops it. The highlight is not persisted across plugin restarts.
- Each key shows a speaker icon over its name. Unlabelled keys are named `Preset 1`, `Preset 2`, … by their position on the deck (reading order: top row left to right, then the next row), not by when they were added, so the numbering stays tidy when keys are moved or removed.
- The property inspector lets you give the key a label (shown in place of `Preset N`), lists what is stored, and can clear the preset.
- To clear a preset from the deck, add the `Clear Preset` action to a key. Press it once to arm: it turns red and reads `Tap the button to clear it`, and every memory key holding a preset is washed in red. Tap the memory key you want to erase; its stored volumes go and its label stays. Press the clear key again, or wait 10 seconds, to cancel. **Hold** the clear key for 3 seconds to erase every preset on the deck at once; it flashes `ALL cleared`.

Recall matches stored entries to live sources by media identity first (a Twitch/YouTube/Kick tab finds its own entry even though every browser tab shares an app name), then by exact app + stream name, then by app alone — so a stored "Spotify at 40%" still applies after the track changes, and a single stored browser level applies to every open tab. Presets live in the key's action settings, so they survive plugin reloads and OpenDeck restarts.

## ToDo

- Support for mini devices (2×3 grid cells)
- Manual setup for specifying which specific app would live on what column

Contributions are welcome!

## Origins and credits

Stream Mixer started on 8 March 2026 as a fork of [mdvictor/opendeck-volume-controller](https://github.com/mdvictor/opendeck-volume-controller) by **Victor Marin**, released under the MIT license. His plugin provided the foundation this one is built on: the PulseAudio sink-input plumbing and event monitor, the keypad grid action with its mute, volume up, and volume down column, the ignored-apps list, the image drawing primitives, and the OpenAction wiring. Much of that code is still here, underneath.

Everything that distinguishes Stream Mixer was added on top in this repository: Stream Deck + dial channels and the LCD layout, touch-to-mute and long-touch dismiss, per-tab browser identification through MPRIS and the Media Session reconciler, Twitch, YouTube, and Kick channel art, XWayland window icon matching, multi-stream app collapsing, scrolling titles, and the volume memory presets. In October 2026 the upstream project independently gained its own dial support and continues under its original name; the two are now separate plugins with separate identities, which is why Stream Mixer uses its own UUIDs.

The full git history, including Victor's original commits, is preserved in this repository. `LICENSE` carries both copyright notices; the MIT terms are unchanged.
