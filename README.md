# backseat

Real-time P2P screen sharing with viewer annotations.

**No OBS required, minimal dependencies.** Both roles run the same binary. NAT traversal via UDP hole-punching (STUN). A lightweight rendezvous server issues short room codes and helps peers find each other — it never touches video or annotation data.

> **TODO:** add a screenshot or GIF here

---

## Platform support

| Platform | Host | Viewer |
|----------|------|--------|
| Linux (X11) | ✅ | ✅ |
| Linux (Wayland) | ❓ | ❓ |
| Windows | ✅ | ✅ |

---

## Build from source

### Linux

**1. Install system dependencies (once per machine):**

```bash
./scripts/setup-ffmpeg.sh
```

This installs the required apt packages (`libdbus-1-dev`, `libopus-dev`, `libasound2-dev`, `nasm`, `libva-dev`, `libx264-dev`, `mingw-w64`) and adds the Windows cross-compile target to rustup.

**2. Build:**

```bash
cargo build -p overlay            # debug
cargo build --release -p overlay  # release
```

The first build compiles FFmpeg 7.1 and libx264 from source (~10 minutes). Subsequent builds are fast — Cargo caches the compiled output in `target/`.

### Windows (cross-compile from Linux)

After running `setup-ffmpeg.sh`:

```bash
./scripts/cross-windows.sh             # debug
./scripts/cross-windows.sh --release   # release
```

Output: `target/x86_64-pc-windows-gnu/{debug,release}/overlay.exe`

The first run clones and compiles FFmpeg + x264 for Windows (~10 minutes). Subsequent runs are fast. The resulting `.exe` is fully self-contained — no redistributable or runtime DLL is required on the target machine.

> **Note:** this build encodes video in software only. `cross-windows.sh` configures FFmpeg with
> `--disable-everything --enable-encoder=libx264`, so the GPU encoders (NVENC, Quick Sync, AMF) are
> not compiled in and cannot be used at runtime. For hardware encoding on Windows, build natively
> (below). The app logs which encoder it chose at startup.

### Windows (native)

Run the setup script from the repo root (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\setup-ffmpeg.ps1
```

Then open a new terminal and:

```powershell
cargo build -p overlay
cargo build --release -p overlay
```

---

## Usage

Launch the binary. A small window appears asking you to pick a role.

### Hosting

1. Click **Host**.
2. Choose your settings:
   - **Audio source** — None, Microphone (with a device picker), or Desktop audio.
     Desktop audio uses WASAPI loopback on Windows and a PulseAudio/PipeWire monitor source on
     Linux; it is greyed out if no monitor source is found.
   - **Frame rate** — 15, 24, 30, or 60 fps.
   - **Stream quality** — Low, Medium, High, or Auto.
3. Click **Start Hosting** and wait a moment while your public address is discovered.
4. A room code appears in the corner of your screen (e.g. `ABCXYZ`).
5. Share the code with the viewer. A system-tray icon also shows it, with **Copy room code** and
   **Exit** actions.

Your screen is now being streamed. Viewer annotations appear as coloured strokes on your desktop.

None of these settings can be changed once hosting has started — go back to the start screen to
adjust them.

### Joining

1. Click **Join**.
2. Enter your name and the room code the host gave you.
3. Press **Connect** (or Enter).
4. The host's screen appears fullscreen. Use the toolbar at the bottom to draw.

**Toolbar:**

| Control | What it does |
|---------|--------------|
| ✏ Pen / 🖍 Marker | Solid stroke, or a wide semi-transparent highlighter |
| Colour palette | 8 colours |
| S / M / L | Brush size |
| ⌫ Eraser | Deletes whole strokes you drew, under a circular cursor |
| ↖ Select | Move, resize, or delete a sticker you placed |
| 🖼 Sticker | Upload an image (PNG/JPG/WebP/BMP/GIF), max 10 per viewer |
| 🗑 Clear | Removes all of *your* annotations — other viewers' are untouched |
| ⚙ | Toggles a live stats overlay (throughput, packet loss, ping) |

Each viewer sees only their own annotations. Everything appears together on the host's screen.

### Room code formats

| Format | When used |
|--------|-----------|
| `ABCXYZ` (6 uppercase letters) | Rendezvous server available — easiest |
| `203.0.113.42:47474` | Direct IP:port — same LAN or port-forwarded host |
| `127.0.0.1:47474` | Same machine |

---

## Security

**A room code is a password. Anyone who has it can watch your screen and draw on it.**

There is no approval step — the host accepts any viewer that connects with a valid code, does not
prompt you, and offers no way to remove one once connected. Share codes accordingly, and stop
hosting when you're done.

The rendezvous server only exchanges IP addresses; video, audio, and annotations always travel
directly between peers and never pass through it. That traffic is **not encrypted** — treat it as
you would any unencrypted connection over an untrusted network.

Codes are single-use in practice: rooms expire after 10 minutes of host inactivity, and a new code
is issued each time you start hosting.

---

## Configuration

| Variable | Effect |
|----------|--------|
| `BACKSEAT_SERVER` | Rendezvous server URL. Defaults to `https://backseat.fly.dev`. Set to an empty string to disable it entirely and use only direct `IP:port` codes. |
| `BACKSEAT_KF_SECS` | Keyframe interval in seconds (host only). Defaults to `2.0`. Lower means faster recovery from packet loss at the cost of bandwidth. |
| `RUST_LOG` | Log verbosity, e.g. `overlay=debug`. Defaults to `overlay=info`. |

These can also be set in a `.env` file at the repo root.

**Logs:** Linux debug builds print to stdout. Release builds — and all Windows builds, which have no
console — write to `~/.local/share/backseat/backseat.log` or `%APPDATA%\backseat\backseat.log`.
The file is not rotated, so `RUST_LOG=overlay=debug` over a long session will grow it steadily.

---

## Licence

[AGPL-3.0](LICENSE).

The binary statically links FFmpeg and x264, both built under GPL terms, so distributed builds are
covered by the GPL family as well.
