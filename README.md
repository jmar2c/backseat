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
2. Wait a moment while your public address is discovered.
3. A room code appears in the corner of your screen (e.g. `ABCXYZ`).
4. Share the code with the viewer. A system-tray icon also shows it with a **Copy** button.

Your screen is now being streamed. Viewer annotations appear as coloured strokes on your desktop.

### Joining

1. Click **Join**.
2. Enter your name and the room code the host gave you.
3. Press **Connect** (or Enter).
4. The host's screen appears fullscreen. Use the toolbar at the bottom to draw.

**Toolbar:** pen / marker / eraser, 8-colour palette, S/M/L brush size, clear-all.

### Room code formats

| Format | When used |
|--------|-----------|
| `ABCXYZ` (6 uppercase letters) | Rendezvous server available — easiest |
| `203.0.113.42:47474` | Direct IP:port — same LAN or port-forwarded host |
| `127.0.0.1:47474` | Same machine |
