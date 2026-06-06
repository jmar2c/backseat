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

## Requirements

**Linux:** `libvpx7` and `libdbus-1` must be present. `libdbus-1` is pre-installed on most desktops; `libvpx7` may not be:

```bash
sudo apt install libvpx7
```

**Windows:** [Visual C++ Redistributable 2015–2022](https://aka.ms/vs/17/release/vc_redist.x64.exe) must be installed.

### Build from source

**System dependencies (Linux):**

```bash
sudo apt install libdbus-1-dev   # system tray (ksni)
sudo apt install libvpx-dev      # VP8 encoder/decoder
```

```bash
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
