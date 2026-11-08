# tinycord

A voice-only Discord client for Windows, in the spirit of old TeamSpeak.
A small native window, plus an always-on-top "who's talking" overlay.
It has no text chat, no images, and no browser engine.

> **Warning:** Discord doesn't allow third-party clients on normal accounts.
> Using one can get the account banned. Use it at your own risk.

## Download

Get `tinycord.exe` from this repo's **Releases → latest**. It's a single file
and doesn't need an installer.

## First run

1. Open `tinycord.exe`.
2. Paste your Discord token into the box and press **Log in**.
   The token is saved encrypted with Windows DPAPI in `%APPDATA%\tinycord`,
   so only your Windows account on this PC can read it.
3. Pick a server, then **double-click a voice channel** (or select it and press
   **Join**).

To log out, delete `%APPDATA%\tinycord\token.bin`.

## Controls

- **Mute / Deafen**: these work like Discord's buttons. Others see the icons.
- **Voice activation / Push-to-talk / Open mic**: for push-to-talk, press the
  **PTT key** button, then the key or mouse button you want (Mouse 4/5 work).
- **Mic sens**: how loud you need to be for voice activation to open the mic.
- **Overlay**: where the speaker list sits. **Speakers only** hides people who
  aren't talking.
- **Keep on top**: pins the main window too.

## Watching a screen share (Go Live)

- People who are streaming have a red **LIVE** tag next to their name.
- **Double-click** them, or select them and press **Watch**. If you aren't in
  their channel yet, tinycord joins it first.
- The stream opens in its own window, and its sound plays through your
  speakers. Move the mouse over it to show the control bar:
  - **Pin**: keeps the stream window on top of other windows (and games in
    Windowed Fullscreen). Shortcut: **T**.
  - **Volume** slider: 0-200% (stream sound only). Also mouse wheel or
    **Up/Down**; **M** mutes and unmutes.
  - **Fullscreen**: also double-click or **F11**; **Esc** exits.
  - The bar hides after a few seconds. Pin and volume are remembered.
  - Close the window to stop watching.
- Only **H.264** streams can be shown for now. If a stream uses another codec,
  the window says so.
- The video decoder only runs while the window is open.

## Overlay over Valorant

Set Valorant's display mode to **Windowed Fullscreen**. Windows doesn't let
other windows draw over a game in exclusive **Fullscreen** mode.

The overlay is a separate click-through window. It doesn't read or inject into
the game.

## Why voice is less delayed

- The jitter buffer is 40 ms (Songbird's default is 100 ms).
- Incoming audio is capped at 80 ms of backlog. If it piles up, the old audio
  is dropped instead of playing late.
- Mic audio waiting to be sent is capped at 60 ms.

## How it's built

- Rust, plain Win32 UI (`windows-sys`), audio via `cpal`.
- Voice uses [Songbird](https://github.com/serenity-rs/songbird) 0.6.1 with
  DAVE end-to-end encryption via the pure-Rust `davey` crate.
- `vendor/songbird` has one patch: Discord may not tell user accounts who
  joined the voice session (songbird issue #310), which made DAVE drop other
  people's audio. tinycord passes the member list it already has from the
  main connection.
- Stream video: H.264 RTP reassembly (`src/video.rs`), DAVE video
  decryption via `davey`, decoding with OpenH264 (built from source). Songbird
  is patched to run a viewer-only video connection (`Config::video_receive`),
  hand video packets to tinycord, and send RTCP keyframe requests.
- GitHub Actions builds the `.exe` on each push to `main`.
