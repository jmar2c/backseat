# screenshare-coach

Real-time collaborative screen annotation tool.  
Friends join a URL, see your screen via OBS WebRTC, and draw/point on it — their annotations appear as an overlay on your desktop.

## Architecture

```text
OBS (WebRTC) ──► server/ (Axum) ◄──► client/ (React + Fabric.js)
                      │
                      ▼
                 overlay/ (egui)   ← renders annotations on your screen
```

## Crates / Packages

| Path        | Language | Purpose                                      |
|-------------|----------|----------------------------------------------|
| `server/`   | Rust     | Axum HTTP + WebSocket hub, WebRTC signaling  |
| `overlay/`  | Rust     | egui/eframe transparent desktop overlay      |
| `shared/`   | Rust     | Message types shared by server + overlay     |
| `client/`   | TS/React | Browser client — video stream + Fabric canvas|

## Quick Start

### 1. Server

```bash
cp .env.example .env
cargo run -p server
```

### 2. Overlay (your PC)

```bash
cargo run -p overlay
```

### 3. Client (browser)

```bash
cd client
npm install
npm run dev
# open http://localhost:5173
```

### 4. OBS

- Add a Browser Source pointing at the signaling URL the server prints on startup
- Or use OBS Virtual Camera + a screen capture pipeline (see docs)

## Milestone Plan

1. **WS ping** — server ↔ overlay exchange heartbeats ✅ (scaffolded)
2. **Cursor relay** — mouse position from client → server → overlay renders a dot
3. **Draw relay** — Fabric strokes → server → overlay paints them
4. **WebRTC stream** — OBS → server (signaling) → client sees your screen
5. **Rooms + auth** — room codes, named users, per-user colours
6. **Polish** — voting overlay, draw tools, replay
