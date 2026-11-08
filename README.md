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

## Sharing your screen (Go Live)

1. Join a voice channel and press **Go Live** (next to Deafen).
2. Pick the **Screen**, **Resolution** and **FPS**, and whether to
   **Share PC sound**.
3. Press **Go Live**. People in the channel see the red LIVE tag next to you
   and can watch from Discord.
4. **Stop Live** (main window) or **Stop** (Go Live window) ends it.

- **Resolution and FPS** only list what Discord allows you: without Nitro,
  720p at 30 fps; a server with boost level 1 allows 720p 60, level 2 or 3
  allows 1080p 60; Nitro Basic allows 1080p 60; Nitro allows any size at 60.
  You get whichever is higher of your account and the server. Discord has
  no setting above 60 fps.
- You can change resolution, FPS or sound while live; viewers see a short
  refresh.
- **PC sound** is everything your PC plays except tinycord itself, so
  viewers don't hear the voice chat twice. Needs Windows 10 (2004) or newer.
- The status line shows viewers, picture size, frames per second and bitrate.
- **Valorant:** use **Windowed Fullscreen** so the screen capture can see it.
- The picture is encoded on your **CPU** (OpenH264). 720p 30 is light;
  1080p 60 takes a real share of a laptop CPU, which games also want.
- If viewers' connections drop packets, tinycord resends them and lowers
  the bitrate a little; it raises it again when things are clean.
- Screen capture, encoder and buffers only exist while you're live. When you
  stop, they're freed.

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
- Go Live sending (`src/golive.rs`): DXGI Desktop Duplication for the screen
  (`src/capture.rs`), scaling and colour conversion (`src/convert.rs`),
  OpenH264 encoding with packet-sized slices, RTP packetizing with
  retransmission and sender reports (`src/rtp.rs`), DAVE encryption, and
  WASAPI process loopback for PC sound (`src/sysaudio.rs`) encoded with Opus.
- GitHub Actions builds the `.exe` on each push to `main`.
