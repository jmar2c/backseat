//! UDP transport layer: packet framing, fragment reassembly, and STUN discovery.
//!
//! Packet layout:
//! ```text
//! [0x01]                                                    PKT_PUNCH
//! [0x02][RTP-12][idx:u16be][total:u16be][flags:u8][data…]   PKT_VIDEO  (H.264 Annex B fragment)
//! [0x03][utf-8 json…]                                       PKT_ANNOT
//! [0x04]                                                    PKT_DISCONNECT
//! [0x05][RTP-12][opus-data…]                                PKT_AUDIO  (Opus frame)
//! [0x06][video_ts:u32be][audio_ts:u32be][ntp_ms:u64be]      PKT_SYNC   (A/V clock anchor)
//! [0x0C][loss_pct:f32be][ping_ms:f32be]                     PKT_STATS  (viewer → host, for ABR)
//! ```
//!
//! Each RTP header is 12 bytes (RFC 3550):
//! ```text
//! [V=2|P=0|X=0|CC=0][M|PT][seq:u16be][timestamp:u32be][ssrc:u32be]
//! ```
//! H.264 uses PT=96 (90 kHz clock); Opus uses PT=111 (48 kHz clock).
//! For PKT_VIDEO the RTP header is followed by idx/total/flags for reassembly.
//! Frames are chunked to 1 200 bytes to stay well below typical path MTU.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use socket2::{Domain, Protocol, Socket, Type};
use crc32fast;

const PKT_PUNCH:          u8 = 0x01;
const PKT_VIDEO:          u8 = 0x02;
const PKT_ANNOT:          u8 = 0x03;
const PKT_DISCONNECT:     u8 = 0x04;
const PKT_AUDIO:          u8 = 0x05;
const PKT_SYNC:           u8 = 0x06;
const PKT_IMAGE_CHUNK:    u8 = 0x07;
const PKT_IMAGE_MANIFEST: u8 = 0x08;
const PKT_IMAGE_NACK:     u8 = 0x09;
const PKT_PING:           u8 = 0x0A;
const PKT_PONG:           u8 = 0x0B;
const PKT_STATS:          u8 = 0x0C;

const RTP_PT_H264: u8 = 96;
const RTP_PT_OPUS: u8 = 111;

/// Maximum payload per UDP datagram — chosen to stay under typical path MTU.
pub(crate) const CHUNK: usize = 1_200;

/// Frames at or below this size are sent without pacing: the receiver's 4 MB
/// socket buffer absorbs the whole burst, so throttling only adds latency.
const PACE_THRESHOLD_BYTES: usize = 1_024 * 1_024;
/// Target wire rate once pacing engages, in bytes per second.
const PACE_BYTES_PER_SEC: u64 = 50 * 1_024 * 1_024;
/// Minimum accrued budget before we actually sleep.  Windows' timer granularity
/// is ~15.6 ms, so many short sleeps cost far more wall-clock than a few longer
/// ones; batching keeps the granularity penalty bounded.
const PACE_MIN_SLEEP: Duration = Duration::from_millis(5);

/// What the viewer typed into the room-code box.
pub enum RoomCode {
    /// 6-letter code issued by the rendezvous server — requires signaling to resolve.
    Signaling(String),
    /// Direct `IP:port` — used as-is (port-forwarding / same-LAN mode).
    Direct(SocketAddr),
}

/// Shared UDP socket used for all send/receive operations.
pub struct Transport {
    pub socket: Arc<UdpSocket>,
    video_seq:  AtomicU32,
    audio_seq:  AtomicU32,
    ssrc:       u32,
}

/// A decoded, typed packet as returned by [`Transport::recv`].
pub enum Packet {
    Punch,
    VideoFrag {
        rtp_ts:     u32,
        seq:        u16,
        frag_idx:   u16,
        frag_total: u16,
        keyframe:   bool,
        data:       Vec<u8>,
    },
    Annot(String),
    Disconnect,
    Audio { _seq: u16, rtp_ts: u32, data: Vec<u8> },
    Sync  { video_ts: u32, audio_ts: u32, ntp_ms: u64 },
    // ── Sticker image transfer ──────────────────────────────────────────────
    /// One fragment of a sticker image with a CRC32 for per-chunk integrity.
    ImageChunk    { sticker_id: u64, total: u16, idx: u16, crc32: u32, data: Vec<u8> },
    /// Declares the expected SHA-256 and initial placement for a sticker.
    /// Layout: [0x08][id:8][total:2][pos_x:4][pos_y:4][w:4][h:4][sha256:32]
    ImageManifest { sticker_id: u64, total_chunks: u16, pos_x: f32, pos_y: f32, size_w: f32, size_h: f32, sha256: [u8; 32] },
    /// Host → viewer: list of chunk indices to retransmit.
    ImageNack     { sticker_id: u64, missing: Vec<u16> },
    /// Viewer → host: RTT probe with caller's timestamp (ms since UNIX epoch).
    Ping { sent_ms: u64 },
    /// Host → viewer: echo of the Ping timestamp for RTT calculation.
    Pong { sent_ms: u64 },
    /// Viewer → host: network statistics used by the ABR loop.
    Stats { loss_pct: f32, _ping_ms: f32 },  // reserved for future use
}

fn gen_ssrc() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
        .unwrap_or(0x42_42_42_42)
}

/// Create a non-blocking UDP socket bound to `addr` with a 4 MB receive buffer.
/// Large IDR frames can be 500 KB+ spread over 400+ fragments; the Linux default
/// receive buffer (~212 KB) is too small to absorb the burst without dropping packets.
fn udp_socket_with_large_buf(addr: SocketAddr) -> Result<UdpSocket, String> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| e.to_string())?;
    sock.set_reuse_address(true).ok();
    // Request 4 MB; the OS will cap at its maximum (rmem_max on Linux, typically 8 MB+).
    // This is enough to buffer a full 500 KB IDR burst without dropping fragments.
    if let Err(e) = sock.set_recv_buffer_size(4 * 1024 * 1024) {
        tracing::debug!("SO_RCVBUF: {e} (OS cap may be lower; continuing)");
    }
    sock.bind(&addr.into()).map_err(|e| e.to_string())?;
    sock.set_nonblocking(true).map_err(|e| e.to_string())?;
    UdpSocket::from_std(sock.into()).map_err(|e| e.to_string())
}

impl Transport {
    /// Bind to the fixed backseat port on all interfaces (host mode).
    pub async fn bind() -> Result<Self, String> {
        let s = udp_socket_with_large_buf("0.0.0.0:47474".parse().unwrap())?;
        Ok(Self {
            socket:    Arc::new(s),
            video_seq: AtomicU32::new(0),
            audio_seq: AtomicU32::new(0),
            ssrc:      gen_ssrc(),
        })
    }

    /// Bind to an OS-assigned ephemeral port (viewer mode).
    pub async fn bind_ephemeral() -> Result<Self, String> {
        let s = udp_socket_with_large_buf("0.0.0.0:0".parse().unwrap())?;
        Ok(Self {
            socket:    Arc::new(s),
            video_seq: AtomicU32::new(0),
            audio_seq: AtomicU32::new(0),
            ssrc:      gen_ssrc(),
        })
    }

    /// Query the STUN server to learn the public-facing `IP:port` of this socket.
    pub async fn public_addr(&self) -> Option<SocketAddr> {
        gather_public_addr(&self.socket).await
    }

    /// Format a socket address as a human-readable room code.
    pub fn room_code(addr: SocketAddr) -> String {
        addr.to_string()
    }

    /// Parse whatever the viewer typed.
    pub fn parse_room_code(s: &str) -> Option<RoomCode> {
        let s = s.trim();
        let upper = s.to_ascii_uppercase();
        if upper.len() == 6 && upper.chars().all(|c| c.is_ascii_uppercase()) {
            return Some(RoomCode::Signaling(upper));
        }
        s.parse::<SocketAddr>().ok().map(RoomCode::Direct)
    }

    /// Send a single-byte NAT punch packet.
    pub async fn send_punch(&self, to: SocketAddr) -> std::io::Result<()> {
        self.socket.send_to(&[PKT_PUNCH], to).await.map(|_| ())
    }

    /// Fragment `data` into `CHUNK`-byte pieces, wrap each in an RTP header, and send.
    ///
    /// `rtp_ts` is the 90 kHz presentation timestamp from the H.264 encoder.
    /// The RTP marker bit is set on the last fragment of each frame.
    ///
    /// Frames needing more than [`MAX_FRAGS_PER_FRAME`] fragments are dropped here
    /// rather than sent: the receiver rejects such frames outright, so transmitting
    /// them wastes bandwidth and — because the largest frames are always keyframes —
    /// would leave the viewer unable to bootstrap with no indication of why.
    pub async fn send_video(
        &self, to: SocketAddr, rtp_ts: u32, data: &[u8], keyframe: bool,
    ) -> std::io::Result<()> {
        let chunks: Vec<&[u8]> = data.chunks(CHUNK).collect();
        if chunks.len() > MAX_FRAGS_PER_FRAME as usize {
            tracing::error!(
                "frame of {} bytes needs {} fragments, over the {MAX_FRAGS_PER_FRAME} limit \
                 — dropping (lower the bitrate or resolution)",
                data.len(), chunks.len(),
            );
            return Ok(());
        }
        let total    = chunks.len() as u16;
        // Reserve `total` consecutive sequence numbers atomically.
        let base_seq = self.video_seq.fetch_add(total as u32, Ordering::Relaxed) as u16;

        // Pacing state — see the comment at the end of the loop.
        let paced        = data.len() > PACE_THRESHOLD_BYTES;
        let mut deadline = tokio::time::Instant::now();

        for (i, chunk) in chunks.iter().enumerate() {
            let seq    = base_seq.wrapping_add(i as u16);
            let marker = i + 1 == chunks.len();
            let mut pkt = Vec::with_capacity(19 + chunk.len());
            pkt.push(PKT_VIDEO);
            // 12-byte RTP header
            pkt.push(0x80); // V=2, P=0, X=0, CC=0
            pkt.push(RTP_PT_H264 | if marker { 0x80 } else { 0x00 }); // M|PT
            pkt.extend_from_slice(&seq.to_be_bytes());
            pkt.extend_from_slice(&rtp_ts.to_be_bytes());
            pkt.extend_from_slice(&self.ssrc.to_be_bytes());
            // Fragmentation metadata
            pkt.extend_from_slice(&(i as u16).to_be_bytes()); // idx
            pkt.extend_from_slice(&total.to_be_bytes());
            pkt.push(if keyframe { 0x01 } else { 0x00 }); // flags: set on every fragment of a keyframe
            pkt.extend_from_slice(chunk);
            let pkt_len = pkt.len();
            self.socket.send_to(&pkt, to).await?;

            // Pace only genuinely huge frames.  The receiver's 4 MB socket buffer
            // (see `udp_socket_with_large_buf`) absorbs any burst below
            // PACE_THRESHOLD_BYTES without loss, so typical IDRs are sent flat out
            // and never sleep at all.
            //
            // Past that threshold we pace against a running deadline rather than
            // sleeping a fixed interval every N fragments.  Fixed-interval sleeping
            // quantises to the OS timer granularity — ~15.6 ms on Windows — so a
            // frame needing ten 1 ms sleeps stalls for ~160 ms instead of ~10 ms.
            // Sleeping only once the accrued budget exceeds PACE_MIN_SLEEP keeps the
            // number of sleeps (and therefore the granularity penalty) small, and
            // resetting the deadline when we fall behind stops burst credit from
            // accumulating.
            if paced {
                deadline += Duration::from_nanos(
                    pkt_len as u64 * 1_000_000_000 / PACE_BYTES_PER_SEC,
                );
                let now = tokio::time::Instant::now();
                if deadline.saturating_duration_since(now) >= PACE_MIN_SLEEP {
                    tokio::time::sleep_until(deadline).await;
                } else if now > deadline {
                    deadline = now;
                }
            }
        }
        Ok(())
    }

    /// Send one Opus-encoded audio frame wrapped in an RTP header.
    ///
    /// `rtp_ts` is the 48 kHz sample counter (increments by 960 per 20 ms frame).
    pub async fn send_audio(
        &self, to: SocketAddr, rtp_ts: u32, data: &[u8],
    ) -> std::io::Result<()> {
        let seq = self.audio_seq.fetch_add(1, Ordering::Relaxed) as u16;
        let mut pkt = Vec::with_capacity(14 + data.len());
        pkt.push(PKT_AUDIO);
        pkt.push(0x80); // V=2, P=0, X=0, CC=0
        pkt.push(RTP_PT_OPUS | 0x80); // M=1 (complete frame), PT=111
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.extend_from_slice(&rtp_ts.to_be_bytes());
        pkt.extend_from_slice(&self.ssrc.to_be_bytes());
        pkt.extend_from_slice(data);
        self.socket.send_to(&pkt, to).await.map(|_| ())
    }

    /// Send a clock-anchor packet so the viewer can synchronise audio and video playback.
    ///
    /// `video_ts` and `audio_ts` are the last RTP timestamps sent on each stream.
    /// `ntp_ms` is the corresponding wall-clock time in milliseconds since the Unix epoch.
    pub async fn send_sync(
        &self, to: SocketAddr, video_ts: u32, audio_ts: u32, ntp_ms: u64,
    ) -> std::io::Result<()> {
        let mut pkt = [0u8; 17];
        pkt[0] = PKT_SYNC;
        pkt[1..5].copy_from_slice(&video_ts.to_be_bytes());
        pkt[5..9].copy_from_slice(&audio_ts.to_be_bytes());
        pkt[9..17].copy_from_slice(&ntp_ms.to_be_bytes());
        self.socket.send_to(&pkt, to).await.map(|_| ())
    }

    /// Send a JSON annotation message as a `PKT_ANNOT` datagram.
    pub async fn send_annot(&self, to: SocketAddr, json: &str) -> std::io::Result<()> {
        let mut pkt = vec![PKT_ANNOT];
        pkt.extend_from_slice(json.as_bytes());
        self.socket.send_to(&pkt, to).await.map(|_| ())
    }

    /// Wait for the next incoming datagram and parse it into a [`Packet`].
    /// Returns `(src, packet, byte_count)` so callers can track bandwidth.
    pub async fn recv(&self, buf: &mut Vec<u8>) -> Option<(SocketAddr, Packet, usize)> {
        buf.resize(65_536, 0);
        let (n, from) = match self.socket.recv_from(buf).await {
            Ok(v)  => v,
            Err(e) => { tracing::warn!("recv_from error: {e}"); return None; }
        };
        let data = &buf[..n];
        if data.is_empty() { return None; }
        tracing::trace!("udp rx {n}B from {from} type=0x{:02x}", data[0]);
        let pkt = parse_packet(data)?;
        Some((from, pkt, n))
    }

    /// Send a round-trip probe to `to` with the caller's timestamp.
    pub async fn send_ping(&self, to: SocketAddr, sent_ms: u64) -> std::io::Result<()> {
        let mut pkt = [0u8; 9];
        pkt[0] = PKT_PING;
        pkt[1..9].copy_from_slice(&sent_ms.to_be_bytes());
        self.socket.send_to(&pkt, to).await.map(|_| ())
    }

    /// Send viewer network statistics to the host for ABR bitrate adaptation.
    pub async fn send_stats(&self, to: SocketAddr, loss_pct: f32, ping_ms: f32) -> std::io::Result<()> {
        let mut pkt = [0u8; 9];
        pkt[0] = PKT_STATS;
        pkt[1..5].copy_from_slice(&loss_pct.to_be_bytes());
        pkt[5..9].copy_from_slice(&ping_ms.to_be_bytes());
        self.socket.send_to(&pkt, to).await.map(|_| ())
    }

    /// Echo a Ping back as a Pong (host-side response).
    pub async fn send_pong(&self, to: SocketAddr, sent_ms: u64) -> std::io::Result<()> {
        let mut pkt = [0u8; 9];
        pkt[0] = PKT_PONG;
        pkt[1..9].copy_from_slice(&sent_ms.to_be_bytes());
        self.socket.send_to(&pkt, to).await.map(|_| ())
    }

    /// Fragment image bytes into CHUNK-sized pieces and send each with a CRC32.
    pub async fn send_image_chunks(
        &self, to: SocketAddr, sticker_id: u64, bytes: &[u8],
    ) -> std::io::Result<()> {
        let chunks: Vec<&[u8]> = bytes.chunks(CHUNK).collect();
        let total = chunks.len() as u16;
        for (i, chunk) in chunks.iter().enumerate() {
            let crc = crc32fast::hash(chunk);
            let mut pkt = Vec::with_capacity(17 + chunk.len());
            pkt.push(PKT_IMAGE_CHUNK);
            pkt.extend_from_slice(&sticker_id.to_be_bytes());
            pkt.extend_from_slice(&total.to_be_bytes());
            pkt.extend_from_slice(&(i as u16).to_be_bytes());
            pkt.extend_from_slice(&crc.to_be_bytes());
            pkt.extend_from_slice(chunk);
            self.socket.send_to(&pkt, to).await?;
        }
        Ok(())
    }

    /// Send the manifest declaring a sticker's total chunk count, placement, and SHA-256.
    pub async fn send_image_manifest(
        &self, to: SocketAddr, sticker_id: u64, total_chunks: u16,
        pos_x: f32, pos_y: f32, size_w: f32, size_h: f32, sha256: &[u8; 32],
    ) -> std::io::Result<()> {
        let mut pkt = [0u8; 59];
        pkt[0] = PKT_IMAGE_MANIFEST;
        pkt[1..9].copy_from_slice(&sticker_id.to_be_bytes());
        pkt[9..11].copy_from_slice(&total_chunks.to_be_bytes());
        pkt[11..15].copy_from_slice(&pos_x.to_be_bytes());
        pkt[15..19].copy_from_slice(&pos_y.to_be_bytes());
        pkt[19..23].copy_from_slice(&size_w.to_be_bytes());
        pkt[23..27].copy_from_slice(&size_h.to_be_bytes());
        pkt[27..59].copy_from_slice(sha256);
        self.socket.send_to(&pkt, to).await.map(|_| ())
    }

    /// Send a NACK listing chunk indices that need retransmission.
    pub async fn send_image_nack(
        &self, to: SocketAddr, sticker_id: u64, missing: &[u16],
    ) -> std::io::Result<()> {
        let count = missing.len().min(1000) as u16;
        let mut pkt = Vec::with_capacity(11 + count as usize * 2);
        pkt.push(PKT_IMAGE_NACK);
        pkt.extend_from_slice(&sticker_id.to_be_bytes());
        pkt.extend_from_slice(&count.to_be_bytes());
        for &idx in &missing[..count as usize] {
            pkt.extend_from_slice(&idx.to_be_bytes());
        }
        self.socket.send_to(&pkt, to).await.map(|_| ())
    }
}

/// Parse a raw datagram buffer into a typed [`Packet`].
/// Returns `None` for unknown types, malformed packets, or invalid UTF-8 in annotations.
pub(crate) fn parse_packet(data: &[u8]) -> Option<Packet> {
    if data.is_empty() { return None; }

    let pkt = match data[0] {
        PKT_PUNCH => Packet::Punch,

        // Layout: [0x02][0x80][M|PT][seq:2][rtp_ts:4][ssrc:4][idx:2][total:2][flags:1][payload…]
        //          0     1     2     3-4     5-8        9-12   13-14  15-16    17       18+
        PKT_VIDEO if data.len() > 18 => {
            let seq        = u16::from_be_bytes([data[3],  data[4]]);
            let rtp_ts     = u32::from_be_bytes([data[5],  data[6],  data[7],  data[8]]);
            // data[9..13] = ssrc (ignored; we trust the sending socket address)
            let frag_idx   = u16::from_be_bytes([data[13], data[14]]);
            let frag_total = u16::from_be_bytes([data[15], data[16]]);
            let keyframe   = data[17] & 0x01 != 0;
            Packet::VideoFrag {
                rtp_ts, seq, frag_idx, frag_total, keyframe,
                data: data[18..].to_vec(),
            }
        }

        PKT_ANNOT if data.len() > 1 => {
            let s = std::str::from_utf8(&data[1..]).ok()?.to_string();
            Packet::Annot(s)
        }

        PKT_DISCONNECT => Packet::Disconnect,

        // Layout: [0x05][0x80][M|PT][seq:2][rtp_ts:4][ssrc:4][payload…]
        //          0     1     2     3-4    5-8        9-12    13+
        PKT_AUDIO if data.len() > 13 => {
            let seq    = u16::from_be_bytes([data[3], data[4]]);
            let rtp_ts = u32::from_be_bytes([data[5], data[6], data[7], data[8]]);
            Packet::Audio { _seq: seq, rtp_ts, data: data[13..].to_vec() }
        }

        // Layout: [0x06][video_ts:4][audio_ts:4][ntp_ms:8]
        PKT_SYNC if data.len() == 17 => {
            let video_ts = u32::from_be_bytes([data[1], data[2], data[3],  data[4]]);
            let audio_ts = u32::from_be_bytes([data[5], data[6], data[7],  data[8]]);
            let ntp_ms   = u64::from_be_bytes([
                data[9], data[10], data[11], data[12],
                data[13], data[14], data[15], data[16],
            ]);
            Packet::Sync { video_ts, audio_ts, ntp_ms }
        }

        // Layout: [0x07][id:8][total:2][idx:2][crc32:4][data…]  min = 17+1 = 18
        PKT_IMAGE_CHUNK if data.len() >= 18 => {
            let sticker_id = u64::from_be_bytes(data[1..9].try_into().ok()?);
            let total      = u16::from_be_bytes([data[9],  data[10]]);
            let idx        = u16::from_be_bytes([data[11], data[12]]);
            let crc32      = u32::from_be_bytes([data[13], data[14], data[15], data[16]]);
            Packet::ImageChunk { sticker_id, total, idx, crc32, data: data[17..].to_vec() }
        }

        // Layout: [0x08][id:8][total:2][pos_x:4][pos_y:4][w:4][h:4][sha256:32]  = 59
        PKT_IMAGE_MANIFEST if data.len() == 59 => {
            let sticker_id   = u64::from_be_bytes(data[1..9].try_into().ok()?);
            let total_chunks = u16::from_be_bytes([data[9], data[10]]);
            let pos_x  = f32::from_be_bytes(data[11..15].try_into().ok()?);
            let pos_y  = f32::from_be_bytes(data[15..19].try_into().ok()?);
            let size_w = f32::from_be_bytes(data[19..23].try_into().ok()?);
            let size_h = f32::from_be_bytes(data[23..27].try_into().ok()?);
            let mut sha256 = [0u8; 32];
            sha256.copy_from_slice(&data[27..59]);
            Packet::ImageManifest { sticker_id, total_chunks, pos_x, pos_y, size_w, size_h, sha256 }
        }

        // Layout: [0x09][id:8][count:2][idx:2 × count]  min = 11
        PKT_IMAGE_NACK if data.len() >= 11 => {
            let sticker_id = u64::from_be_bytes(data[1..9].try_into().ok()?);
            let count      = u16::from_be_bytes([data[9], data[10]]) as usize;
            if data.len() < 11 + count * 2 { return None; }
            let missing = (0..count)
                .map(|i| u16::from_be_bytes([data[11 + i * 2], data[12 + i * 2]]))
                .collect();
            Packet::ImageNack { sticker_id, missing }
        }

        // Layout: [0x0A][sent_ms:8]
        PKT_PING if data.len() == 9 => {
            let sent_ms = u64::from_be_bytes(data[1..9].try_into().ok()?);
            Packet::Ping { sent_ms }
        }

        // Layout: [0x0B][sent_ms:8]
        PKT_PONG if data.len() == 9 => {
            let sent_ms = u64::from_be_bytes(data[1..9].try_into().ok()?);
            Packet::Pong { sent_ms }
        }

        // Layout: [0x0C][loss_pct:4be][ping_ms:4be]
        PKT_STATS if data.len() == 9 => {
            let loss_pct = f32::from_be_bytes(data[1..5].try_into().ok()?);
            let ping_ms  = f32::from_be_bytes(data[5..9].try_into().ok()?);
            Packet::Stats { loss_pct, _ping_ms: ping_ms }
        }

        _ => return None,
    };

    Some(pkt)
}

// ── Fragment reassembler ──────────────────────────────────────────────────────

/// Largest encoded frame we will attempt to reassemble.  A 4K IDR at a high
/// bitrate can exceed 2 MB, so the old 1.2 MB ceiling silently discarded whole
/// keyframes — and since keyframes are always the largest frames, the viewer
/// could never bootstrap.
const MAX_FRAME_BYTES: usize = 8 * 1_024 * 1_024;

/// Maximum fragments accepted per frame, derived from [`MAX_FRAME_BYTES`].
/// [`Transport::send_video`] enforces the same limit before transmitting.
pub(crate) const MAX_FRAGS_PER_FRAME: u16 = (MAX_FRAME_BYTES / CHUNK) as u16;

/// Frames whose timestamp trails the newest seen by more than this are treated
/// as stale and evicted.  180 000 ticks = 2 s on the 90 kHz RTP clock.
const STALE_TICKS: u32 = 180_000;

/// A timestamp further than this from the high-water mark — in either direction —
/// is a discontinuity rather than reordering.  The encoder restarts `pts` at 0
/// whenever it is rebuilt (resolution change), so the stream legitimately jumps
/// backwards; treat that as a resync instead of dropping every frame forever.
const RESYNC_TICKS: u32 = 90_000 * 10; // 10 s

/// Collects fragments for multiple in-flight frames and emits complete frames.
pub struct Reassembler {
    frames: HashMap<u32, PendingFrame>,
    /// Highest RTP timestamp seen so far.  Eviction is measured against this,
    /// never against the incoming fragment's own timestamp.
    max_ts: Option<u32>,
}

struct PendingFrame {
    frags:    HashMap<u16, Vec<u8>>,
    total:    u16,
    keyframe: bool,
}

impl Reassembler {
    pub fn new() -> Self {
        Self { frames: HashMap::new(), max_ts: None }
    }

    /// Feed a fragment. Returns `Some((frame_data, keyframe))` when complete.
    ///
    /// Frames more than [`STALE_TICKS`] behind the highest timestamp seen so far
    /// are evicted to prevent unbounded memory growth.  Eviction deliberately
    /// measures against the high-water mark rather than the incoming fragment's
    /// timestamp: measuring against the latter meant a single reordered fragment
    /// from an older frame wrapped the subtraction for every newer entry and
    /// evicted all of them mid-assembly.
    pub fn push(
        &mut self,
        rtp_ts: u32, frag_idx: u16, frag_total: u16, keyframe: bool, data: Vec<u8>,
    ) -> Option<(Vec<u8>, bool)> {
        if frag_total == 0 || frag_total > MAX_FRAGS_PER_FRAME || frag_idx >= frag_total {
            return None;
        }

        // Advance (or resync) the high-water mark, then evict against it.
        let newest = match self.max_ts {
            None => { self.max_ts = Some(rtp_ts); rtp_ts }
            Some(newest) => {
                // Serial-number arithmetic: `behind >= 2^31` means rtp_ts is
                // actually *ahead* of the mark, taking the short way round.
                let behind = newest.wrapping_sub(rtp_ts);
                if behind >= 0x8000_0000 {
                    if rtp_ts.wrapping_sub(newest) > RESYNC_TICKS {
                        tracing::debug!("rtp_ts jumped forward to {rtp_ts} — resyncing");
                        self.frames.clear();
                    }
                    self.max_ts = Some(rtp_ts);
                    rtp_ts
                } else if behind > RESYNC_TICKS {
                    // Encoder rebuilt and restarted pts at 0.
                    tracing::debug!("rtp_ts restarted at {rtp_ts} — resyncing");
                    self.frames.clear();
                    self.max_ts = Some(rtp_ts);
                    rtp_ts
                } else if behind > STALE_TICKS {
                    return None; // late fragment for an already-evicted frame
                } else {
                    newest // reordered, but still inside the window
                }
            }
        };
        self.frames.retain(|id, _| newest.wrapping_sub(*id) <= STALE_TICKS);

        let entry = self.frames.entry(rtp_ts).or_insert(PendingFrame {
            frags: HashMap::new(), total: frag_total, keyframe: false,
        });
        entry.keyframe |= keyframe; // any fragment of a keyframe can carry the flag
        entry.frags.insert(frag_idx, data);

        if entry.frags.len() == entry.total as usize {
            let e = self.frames.remove(&rtp_ts)?;
            let mut parts: Vec<(u16, Vec<u8>)> = e.frags.into_iter().collect();
            parts.sort_unstable_by_key(|(i, _)| *i);
            let out: Vec<u8> = parts.into_iter().flat_map(|(_, d)| d).collect();
            Some((out, e.keyframe))
        } else {
            None
        }
    }
}

// ── STUN public-address discovery ─────────────────────────────────────────────

pub async fn gather_public_addr(socket: &UdpSocket) -> Option<SocketAddr> {
    stun_query(socket, "stun.l.google.com:19302").await
}

pub async fn diagnose_nat(socket: &UdpSocket) -> Option<String> {
    let a = stun_query(socket, "stun.l.google.com:19302").await?;
    let b = stun_query(socket, "stun1.l.google.com:19302").await?;
    if a.port() != b.port() {
        return Some(format!(
            "Symmetric NAT or VPN detected (STUN ports {}/{}). \
             Hole-punching may fail — try disabling your VPN.",
            a.port(), b.port()
        ));
    }
    None
}

async fn stun_query(socket: &UdpSocket, stun_host: &str) -> Option<SocketAddr> {
    let stun_addr = tokio::net::lookup_host(stun_host).await.ok()?
        .find(|a| a.is_ipv4())?;

    let mut req = [0u8; 20];
    req[0] = 0x00; req[1] = 0x01;
    req[2] = 0x00; req[3] = 0x00;
    req[4] = 0x21; req[5] = 0x12;
    req[6] = 0xA4; req[7] = 0x42;
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos()).unwrap_or(0);
    req[8..12].copy_from_slice(&seed.to_be_bytes());
    req[12..16].copy_from_slice(&(seed ^ 0xDEAD_BEEF).to_be_bytes());
    req[16..20].copy_from_slice(&seed.wrapping_add(0x1234_5678).to_be_bytes());

    socket.send_to(&req, stun_addr).await.ok()?;

    let mut buf = [0u8; 512];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let (n, from) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf))
            .await.ok()?.ok()?;
        if from != stun_addr { continue; }
        let data = &buf[..n];
        if data.len() >= 20 && data[8..20] == req[8..20] {
            return parse_xor_mapped_address(data);
        }
    }
}

fn parse_xor_mapped_address(buf: &[u8]) -> Option<SocketAddr> {
    if buf.len() < 20 { return None; }
    if buf[0] != 0x01 || buf[1] != 0x01 { return None; }
    const MAGIC: u32 = 0x2112_A442;
    let body_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    if buf.len() < 20 + body_len { return None; }
    let mut i = 20;
    while i + 4 <= 20 + body_len {
        let attr_type = u16::from_be_bytes([buf[i],     buf[i + 1]]);
        let attr_len  = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        i += 4;
        if i + attr_len > 20 + body_len { break; }
        if attr_type == 0x0020 && attr_len >= 8 && buf[i + 1] == 0x01 {
            let port = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) ^ (MAGIC >> 16) as u16;
            let ip   = u32::from_be_bytes([buf[i+4], buf[i+5], buf[i+6], buf[i+7]]) ^ MAGIC;
            return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port));
        }
        i += (attr_len + 3) & !3;
    }
    None
}

pub fn discover_lan_ip() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:53").ok()?;
    Some(s.local_addr().ok()?.ip())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Build a PKT_VIDEO buffer the same way send_video does for one fragment.
    fn make_video_pkt(rtp_ts: u32, seq: u16, idx: u16, total: u16, keyframe: bool, payload: &[u8]) -> Vec<u8> {
        let marker = idx + 1 == total;
        let mut pkt = Vec::new();
        pkt.push(PKT_VIDEO);
        pkt.push(0x80);
        pkt.push(RTP_PT_H264 | if marker { 0x80 } else { 0x00 });
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.extend_from_slice(&rtp_ts.to_be_bytes());
        pkt.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes()); // ssrc
        pkt.extend_from_slice(&idx.to_be_bytes());
        pkt.extend_from_slice(&total.to_be_bytes());
        pkt.push(if keyframe { 0x01 } else { 0x00 });
        pkt.extend_from_slice(payload);
        pkt
    }

    // Build a minimal well-formed STUN success response containing an
    // XOR-MAPPED-ADDRESS attribute for the given (port, IPv4 address as u32).
    fn make_stun_response(port: u16, ip: u32) -> Vec<u8> {
        const MAGIC: u32 = 0x2112_A442;
        let xored_port = port ^ (MAGIC >> 16) as u16;
        let xored_ip   = ip  ^ MAGIC;
        // 8-byte XOR-MAPPED-ADDRESS attribute body: reserved | family | port | ip
        let mut attr_body = vec![0x00, 0x01];
        attr_body.extend_from_slice(&xored_port.to_be_bytes());
        attr_body.extend_from_slice(&xored_ip.to_be_bytes());
        let attr_len  = attr_body.len() as u16;  // 8
        let body_len  = 4 + attr_len;            // attr type+len header + body = 12
        // 20-byte STUN message header
        let mut buf = vec![0x01, 0x01]; // success response
        buf.extend_from_slice(&body_len.to_be_bytes());
        buf.extend_from_slice(&MAGIC.to_be_bytes());
        buf.extend_from_slice(&[0u8; 12]); // transaction ID
        // Attribute
        buf.extend_from_slice(&0x0020u16.to_be_bytes()); // XOR-MAPPED-ADDRESS
        buf.extend_from_slice(&attr_len.to_be_bytes());
        buf.extend_from_slice(&attr_body);
        buf
    }

    // ── parse_packet: single-byte control packets ─────────────────────────────

    #[test]
    fn parse_punch() {
        assert!(matches!(parse_packet(&[PKT_PUNCH]), Some(Packet::Punch)));
    }

    #[test]
    fn parse_disconnect() {
        assert!(matches!(parse_packet(&[PKT_DISCONNECT]), Some(Packet::Disconnect)));
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_packet(&[]).is_none());
    }

    #[test]
    fn parse_unknown_type_returns_none() {
        assert!(parse_packet(&[0xFF]).is_none());
        assert!(parse_packet(&[0x00]).is_none());
    }

    // ── parse_packet: PKT_ANNOT ───────────────────────────────────────────────

    #[test]
    fn parse_annot_valid() {
        let json = r#"{"type":"cursor_move"}"#;
        let mut data = vec![PKT_ANNOT];
        data.extend_from_slice(json.as_bytes());
        if let Some(Packet::Annot(s)) = parse_packet(&data) {
            assert_eq!(s, json);
        } else {
            panic!("expected Annot");
        }
    }

    #[test]
    fn parse_annot_empty_payload_returns_none() {
        // len == 1 fails the `data.len() > 1` guard
        assert!(parse_packet(&[PKT_ANNOT]).is_none());
    }

    #[test]
    fn parse_annot_invalid_utf8_returns_none() {
        assert!(parse_packet(&[PKT_ANNOT, 0xFF, 0xFE]).is_none());
    }

    // ── parse_packet: PKT_VIDEO ───────────────────────────────────────────────

    #[test]
    fn parse_video_single_fragment_roundtrip() {
        let payload = b"some h264 data";
        let raw = make_video_pkt(90_000, 7, 0, 1, false, payload);
        if let Some(Packet::VideoFrag { rtp_ts, seq, frag_idx, frag_total, keyframe, data }) = parse_packet(&raw) {
            assert_eq!(rtp_ts,     90_000);
            assert_eq!(seq,        7);
            assert_eq!(frag_idx,   0);
            assert_eq!(frag_total, 1);
            assert!(!keyframe);
            assert_eq!(data, payload);
        } else {
            panic!("expected VideoFrag");
        }
    }

    #[test]
    fn parse_video_keyframe_flag() {
        let raw = make_video_pkt(180_000, 42, 0, 3, true, b"idr");
        if let Some(Packet::VideoFrag { keyframe, frag_idx, frag_total, .. }) = parse_packet(&raw) {
            assert!(keyframe);
            assert_eq!(frag_idx,   0);
            assert_eq!(frag_total, 3);
        } else {
            panic!("expected VideoFrag");
        }
    }

    #[test]
    fn parse_video_no_payload_returns_none() {
        // Header is exactly 18 bytes; `data.len() > 18` requires ≥ 19.
        let raw = make_video_pkt(0, 0, 0, 1, false, &[]);
        assert_eq!(raw.len(), 18, "sanity: header must be 18 bytes");
        assert!(parse_packet(&raw).is_none());
    }

    // ── parse_packet: PKT_AUDIO ───────────────────────────────────────────────

    #[test]
    fn parse_audio_roundtrip() {
        let opus = b"opus frame";
        let mut pkt = vec![PKT_AUDIO, 0x80, RTP_PT_OPUS | 0x80];
        pkt.extend_from_slice(&3u16.to_be_bytes());          // seq
        pkt.extend_from_slice(&48_000u32.to_be_bytes());     // rtp_ts
        pkt.extend_from_slice(&0xABCDu32.to_be_bytes());     // ssrc (ignored)
        pkt.extend_from_slice(opus);
        if let Some(Packet::Audio { _seq, rtp_ts, data }) = parse_packet(&pkt) {
            assert_eq!(_seq,   3);
            assert_eq!(rtp_ts, 48_000);
            assert_eq!(data,   opus);
        } else {
            panic!("expected Audio");
        }
    }

    #[test]
    fn parse_audio_too_short_returns_none() {
        // Exactly 13 bytes = header only; `data.len() > 13` requires ≥ 14.
        let mut pkt = vec![PKT_AUDIO, 0x80, RTP_PT_OPUS | 0x80];
        pkt.extend_from_slice(&0u16.to_be_bytes());
        pkt.extend_from_slice(&0u32.to_be_bytes());
        pkt.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(pkt.len(), 13);
        assert!(parse_packet(&pkt).is_none());
    }

    // ── parse_packet: PKT_SYNC ────────────────────────────────────────────────

    #[test]
    fn parse_sync_roundtrip() {
        let mut pkt = vec![PKT_SYNC];
        pkt.extend_from_slice(&90_000u32.to_be_bytes());
        pkt.extend_from_slice(&48_000u32.to_be_bytes());
        pkt.extend_from_slice(&1_700_000_000_000u64.to_be_bytes());
        assert_eq!(pkt.len(), 17);
        if let Some(Packet::Sync { video_ts, audio_ts, ntp_ms }) = parse_packet(&pkt) {
            assert_eq!(video_ts, 90_000);
            assert_eq!(audio_ts, 48_000);
            assert_eq!(ntp_ms,   1_700_000_000_000);
        } else {
            panic!("expected Sync");
        }
    }

    #[test]
    fn parse_sync_wrong_length_returns_none() {
        // PKT_SYNC requires exactly 17 bytes.
        let mut short = vec![PKT_SYNC; 16];
        short[0] = PKT_SYNC;
        assert!(parse_packet(&short).is_none());

        let mut long = vec![0u8; 18];
        long[0] = PKT_SYNC;
        assert!(parse_packet(&long).is_none());
    }

    // ── parse_packet: PKT_PING / PKT_PONG ────────────────────────────────────

    #[test]
    fn parse_ping_roundtrip() {
        let mut pkt = vec![PKT_PING];
        pkt.extend_from_slice(&1_234_567_890u64.to_be_bytes());
        assert!(matches!(parse_packet(&pkt), Some(Packet::Ping { sent_ms: 1_234_567_890 })));
    }

    #[test]
    fn parse_pong_roundtrip() {
        let mut pkt = vec![PKT_PONG];
        pkt.extend_from_slice(&999_999u64.to_be_bytes());
        assert!(matches!(parse_packet(&pkt), Some(Packet::Pong { sent_ms: 999_999 })));
    }

    #[test]
    fn parse_ping_wrong_length_returns_none() {
        assert!(parse_packet(&[PKT_PING; 8]).is_none());
        assert!(parse_packet(&[PKT_PING; 10]).is_none());
    }

    // ── parse_packet: PKT_STATS ───────────────────────────────────────────────

    #[test]
    fn parse_stats_roundtrip() {
        let mut pkt = vec![PKT_STATS];
        pkt.extend_from_slice(&0.05f32.to_be_bytes());
        pkt.extend_from_slice(&42.0f32.to_be_bytes());
        if let Some(Packet::Stats { loss_pct, _ping_ms }) = parse_packet(&pkt) {
            assert!((loss_pct - 0.05).abs() < 1e-6);
            assert!((_ping_ms - 42.0).abs() < 1e-6);
        } else {
            panic!("expected Stats");
        }
    }

    // ── parse_packet: PKT_IMAGE_CHUNK ─────────────────────────────────────────

    #[test]
    fn parse_image_chunk_roundtrip() {
        let payload = b"chunk bytes";
        let crc = crc32fast::hash(payload);
        let mut pkt = vec![PKT_IMAGE_CHUNK];
        pkt.extend_from_slice(&42u64.to_be_bytes());  // sticker_id
        pkt.extend_from_slice(&5u16.to_be_bytes());   // total
        pkt.extend_from_slice(&2u16.to_be_bytes());   // idx
        pkt.extend_from_slice(&crc.to_be_bytes());    // crc32
        pkt.extend_from_slice(payload);
        if let Some(Packet::ImageChunk { sticker_id, total, idx, crc32, data }) = parse_packet(&pkt) {
            assert_eq!(sticker_id, 42);
            assert_eq!(total,      5);
            assert_eq!(idx,        2);
            assert_eq!(crc32,      crc);
            assert_eq!(data,       payload);
        } else {
            panic!("expected ImageChunk");
        }
    }

    // ── parse_packet: PKT_IMAGE_MANIFEST ─────────────────────────────────────

    #[test]
    fn parse_image_manifest_roundtrip() {
        let sha256 = [0xABu8; 32];
        let mut pkt = vec![PKT_IMAGE_MANIFEST];
        pkt.extend_from_slice(&100u64.to_be_bytes());
        pkt.extend_from_slice(&10u16.to_be_bytes());
        pkt.extend_from_slice(&0.25f32.to_be_bytes());
        pkt.extend_from_slice(&0.75f32.to_be_bytes());
        pkt.extend_from_slice(&0.5f32.to_be_bytes());
        pkt.extend_from_slice(&0.3f32.to_be_bytes());
        pkt.extend_from_slice(&sha256);
        assert_eq!(pkt.len(), 59, "sanity: manifest must be 59 bytes");
        if let Some(Packet::ImageManifest { sticker_id, total_chunks, pos_x, pos_y, size_w, size_h, sha256: s }) = parse_packet(&pkt) {
            assert_eq!(sticker_id,   100);
            assert_eq!(total_chunks, 10);
            assert!((pos_x  - 0.25).abs() < 1e-6);
            assert!((pos_y  - 0.75).abs() < 1e-6);
            assert!((size_w - 0.5 ).abs() < 1e-6);
            assert!((size_h - 0.3 ).abs() < 1e-6);
            assert_eq!(s, sha256);
        } else {
            panic!("expected ImageManifest");
        }
    }

    #[test]
    fn parse_image_manifest_wrong_length_returns_none() {
        let mut pkt = vec![0u8; 58];
        pkt[0] = PKT_IMAGE_MANIFEST;
        assert!(parse_packet(&pkt).is_none());
    }

    // ── parse_packet: PKT_IMAGE_NACK ─────────────────────────────────────────

    #[test]
    fn parse_image_nack_roundtrip() {
        let mut pkt = vec![PKT_IMAGE_NACK];
        pkt.extend_from_slice(&7u64.to_be_bytes());  // sticker_id
        pkt.extend_from_slice(&3u16.to_be_bytes());  // count
        for idx in [1u16, 5, 9] {
            pkt.extend_from_slice(&idx.to_be_bytes());
        }
        if let Some(Packet::ImageNack { sticker_id, missing }) = parse_packet(&pkt) {
            assert_eq!(sticker_id, 7);
            assert_eq!(missing,    vec![1, 5, 9]);
        } else {
            panic!("expected ImageNack");
        }
    }

    #[test]
    fn parse_image_nack_count_exceeds_data_returns_none() {
        // Claims 3 missing indices but only has 1 worth of data.
        let mut pkt = vec![PKT_IMAGE_NACK];
        pkt.extend_from_slice(&7u64.to_be_bytes());
        pkt.extend_from_slice(&3u16.to_be_bytes()); // count=3
        pkt.extend_from_slice(&1u16.to_be_bytes()); // only one index present
        assert!(parse_packet(&pkt).is_none());
    }

    // ── Reassembler ───────────────────────────────────────────────────────────

    #[test]
    fn reassembler_single_fragment_completes_immediately() {
        let mut r = Reassembler::new();
        let data = b"complete frame".to_vec();
        let result = r.push(1000, 0, 1, false, data.clone());
        assert_eq!(result, Some((data, false)));
    }

    #[test]
    fn reassembler_multi_fragment_in_order() {
        let mut r = Reassembler::new();
        assert!(r.push(2000, 0, 3, false, b"aaa".to_vec()).is_none());
        assert!(r.push(2000, 1, 3, false, b"bbb".to_vec()).is_none());
        let (frame, keyframe) = r.push(2000, 2, 3, false, b"ccc".to_vec()).unwrap();
        assert_eq!(frame, b"aaabbbccc");
        assert!(!keyframe);
    }

    #[test]
    fn reassembler_multi_fragment_out_of_order() {
        let mut r = Reassembler::new();
        r.push(3000, 2, 3, false, b"ccc".to_vec());
        r.push(3000, 0, 3, false, b"aaa".to_vec());
        let (frame, _) = r.push(3000, 1, 3, false, b"bbb".to_vec()).unwrap();
        assert_eq!(frame, b"aaabbbccc");
    }

    #[test]
    fn reassembler_keyframe_flag_propagates_from_any_fragment() {
        let mut r = Reassembler::new();
        // Only the first fragment carries the flag; second does not.
        r.push(4000, 0, 2, true, b"idr".to_vec());
        let (_, keyframe) = r.push(4000, 1, 2, false, b"rest".to_vec()).unwrap();
        assert!(keyframe);
    }

    #[test]
    fn reassembler_two_concurrent_frames_do_not_interfere() {
        let mut r = Reassembler::new();
        // Interleave the first fragments of two frames.
        r.push(7000, 0, 2, false, b"a1".to_vec());
        r.push(8000, 0, 2, false, b"b1".to_vec());
        // Complete the higher-ts frame first: pushing ts=8000 keeps ts=7000
        // (8000.wrapping_sub(7000)=1000 ≤ 180_000), so both frames stay alive.
        let (fb, _) = r.push(8000, 1, 2, false, b"b2".to_vec()).unwrap();
        assert_eq!(fb, b"b1b2");
        // ts=8000 has been removed; only ts=7000 remains. Complete it.
        let (fa, _) = r.push(7000, 1, 2, false, b"a2".to_vec()).unwrap();
        assert_eq!(fa, b"a1a2");
    }

    #[test]
    fn reassembler_evicts_stale_frames() {
        let mut r = Reassembler::new();
        // Incomplete frame at ts=1000.
        r.push(1000, 0, 2, false, b"frag0".to_vec());
        // Advance more than 180_000 ticks — ts=1000 is now stale and gets evicted.
        let completed = r.push(181_001, 0, 1, false, b"new".to_vec());
        assert!(completed.is_some(), "single-fragment frame at ts=181_001 should complete");
        // The stale frame's second fragment should not reconstruct anything.
        let stale = r.push(1000, 1, 2, false, b"frag1".to_vec());
        assert!(stale.is_none());
    }

    // Regression: a reordered fragment carrying an *older* timestamp used to
    // wrap the eviction subtraction and destroy every newer in-flight frame.
    #[test]
    fn reassembler_late_fragment_does_not_evict_newer_frames() {
        let mut r = Reassembler::new();
        // Frame A (ts=300_000) is mid-assembly.
        assert!(r.push(300_000, 0, 2, false, b"a1".to_vec()).is_none());
        // A straggler from the previous frame (ts=297_000) arrives late.  Before
        // the fix this evicted frame A: 297_000.wrapping_sub(300_000) is huge.
        assert!(r.push(297_000, 0, 2, false, b"b1".to_vec()).is_none());
        // Frame A must still be assemblable.
        let (frame, _) = r.push(300_000, 1, 2, false, b"a2".to_vec())
            .expect("frame A survived the late fragment");
        assert_eq!(frame, b"a1a2");
    }

    #[test]
    fn reassembler_drops_fragments_older_than_the_stale_window() {
        let mut r = Reassembler::new();
        r.push(500_000, 0, 1, false, b"current".to_vec());
        // More than STALE_TICKS behind the high-water mark, but well inside
        // RESYNC_TICKS — a genuinely late fragment, not a restart.
        assert!(r.push(500_000 - STALE_TICKS - 1, 0, 1, false, b"ancient".to_vec()).is_none());
    }

    // The encoder restarts pts at 0 when rebuilt (e.g. resolution change).  That
    // is a discontinuity, not a late fragment, so the reassembler must resync
    // rather than reject every subsequent frame forever.
    #[test]
    fn reassembler_resyncs_when_encoder_restarts_pts() {
        let mut r = Reassembler::new();
        r.push(90_000 * 60, 0, 1, false, b"before".to_vec());
        let (frame, _) = r.push(0, 0, 1, true, b"after".to_vec())
            .expect("post-restart frame accepted");
        assert_eq!(frame, b"after");
        // And the stream keeps working from the new baseline.
        let (next, _) = r.push(3_000, 0, 1, false, b"next".to_vec()).unwrap();
        assert_eq!(next, b"next");
    }

    #[test]
    fn reassembler_accepts_frames_at_the_fragment_ceiling() {
        let mut r = Reassembler::new();
        // A frame using the full fragment budget must be accepted, not rejected.
        assert!(r.push(1_000, 0, MAX_FRAGS_PER_FRAME, true, b"x".to_vec()).is_none());
        assert!(r.push(1_000, MAX_FRAGS_PER_FRAME - 1, MAX_FRAGS_PER_FRAME, true, b"y".to_vec()).is_none());
    }

    // The sender-side guard in send_video and the receiver-side limit must agree,
    // otherwise frames are transmitted only to be discarded on arrival.
    #[test]
    fn fragment_ceiling_covers_realistic_keyframes() {
        let ceiling_bytes = MAX_FRAGS_PER_FRAME as usize * CHUNK;
        assert!(
            ceiling_bytes >= 4 * 1_024 * 1_024,
            "ceiling {ceiling_bytes} B is too low for a 4K IDR",
        );
        assert_eq!(MAX_FRAGS_PER_FRAME as usize, MAX_FRAME_BYTES / CHUNK);
    }

    #[test]
    fn reassembler_rejects_zero_total() {
        let mut r = Reassembler::new();
        assert!(r.push(5000, 0, 0, false, b"x".to_vec()).is_none());
    }

    #[test]
    fn reassembler_rejects_total_over_max() {
        let mut r = Reassembler::new();
        assert!(r.push(5000, 0, MAX_FRAGS_PER_FRAME + 1, false, b"x".to_vec()).is_none());
    }

    #[test]
    fn reassembler_rejects_idx_gte_total() {
        let mut r = Reassembler::new();
        // idx=3, total=3 → idx >= total
        assert!(r.push(6000, 3, 3, false, b"x".to_vec()).is_none());
    }

    // ── parse_room_code ───────────────────────────────────────────────────────

    #[test]
    fn parse_room_code_6_letter_uppercase() {
        assert!(matches!(
            Transport::parse_room_code("ABCXYZ"),
            Some(RoomCode::Signaling(s)) if s == "ABCXYZ"
        ));
    }

    #[test]
    fn parse_room_code_6_letter_lowercase_normalised() {
        assert!(matches!(
            Transport::parse_room_code("abcxyz"),
            Some(RoomCode::Signaling(s)) if s == "ABCXYZ"
        ));
    }

    #[test]
    fn parse_room_code_ip_port() {
        assert!(matches!(
            Transport::parse_room_code("127.0.0.1:47474"),
            Some(RoomCode::Direct(_))
        ));
    }

    #[test]
    fn parse_room_code_with_whitespace() {
        // Leading/trailing whitespace should be trimmed.
        assert!(matches!(
            Transport::parse_room_code("  ABCDEF  "),
            Some(RoomCode::Signaling(s)) if s == "ABCDEF"
        ));
    }

    #[test]
    fn parse_room_code_invalid_returns_none() {
        assert!(Transport::parse_room_code("").is_none());
        assert!(Transport::parse_room_code("ABC").is_none());       // too short
        assert!(Transport::parse_room_code("ABCDEFG").is_none());   // too long
        assert!(Transport::parse_room_code("ABC123").is_none());    // has digits
        assert!(Transport::parse_room_code("not-an-addr").is_none());
    }

    // ── parse_xor_mapped_address ──────────────────────────────────────────────

    #[test]
    fn parse_xor_mapped_address_valid() {
        let raw_ip   = u32::from_be_bytes([192, 0, 2, 1]);
        let raw_port = 12345u16;
        let buf      = make_stun_response(raw_port, raw_ip);
        let addr     = parse_xor_mapped_address(&buf).unwrap();
        assert_eq!(addr.port(),         raw_port);
        assert_eq!(addr.ip().to_string(), "192.0.2.1");
    }

    #[test]
    fn parse_xor_mapped_address_too_short_returns_none() {
        assert!(parse_xor_mapped_address(&[0u8; 19]).is_none());
    }

    #[test]
    fn parse_xor_mapped_address_wrong_type_returns_none() {
        let mut buf = make_stun_response(1234, 0x01020304);
        buf[0] = 0x00; // not a success response
        assert!(parse_xor_mapped_address(&buf).is_none());
    }

    #[test]
    fn parse_xor_mapped_address_no_attributes_returns_none() {
        // Well-formed 20-byte header with body_len=0 — no attributes.
        let mut buf = vec![0x01, 0x01, 0x00, 0x00]; // type + body_len=0
        buf.extend_from_slice(&0x2112_A442u32.to_be_bytes());
        buf.extend_from_slice(&[0u8; 12]); // tx id
        assert!(parse_xor_mapped_address(&buf).is_none());
    }
}
