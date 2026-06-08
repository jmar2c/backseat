//! UDP transport layer: packet framing, fragment reassembly, and STUN discovery.
//!
//! Packet layout:
//! ```text
//! [0x01]                                                    PKT_PUNCH
//! [0x02][RTP-12][idx:u16be][total:u16be][flags:u8][data…]   PKT_VIDEO  (VP8 fragment)
//! [0x03][utf-8 json…]                                       PKT_ANNOT
//! [0x04]                                                    PKT_DISCONNECT
//! [0x05][RTP-12][opus-data…]                                PKT_AUDIO  (Opus frame)
//! [0x06][video_ts:u32be][audio_ts:u32be][ntp_ms:u64be]      PKT_SYNC   (A/V clock anchor)
//! ```
//!
//! Each RTP header is 12 bytes (RFC 3550):
//! ```text
//! [V=2|P=0|X=0|CC=0][M|PT][seq:u16be][timestamp:u32be][ssrc:u32be]
//! ```
//! VP8 uses PT=96 (90 kHz clock); Opus uses PT=111 (48 kHz clock).
//! For PKT_VIDEO the RTP header is followed by idx/total/flags for reassembly.
//! Frames are chunked to 1 200 bytes to stay well below typical path MTU.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;

const PKT_PUNCH:      u8 = 0x01;
const PKT_VIDEO:      u8 = 0x02;
const PKT_ANNOT:      u8 = 0x03;
const PKT_DISCONNECT: u8 = 0x04;
const PKT_AUDIO:      u8 = 0x05;
const PKT_SYNC:       u8 = 0x06;

const RTP_PT_VP8:  u8 = 96;
const RTP_PT_OPUS: u8 = 111;

/// Maximum payload per UDP datagram — chosen to stay under typical path MTU.
const CHUNK: usize = 1_200;

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
    Audio { seq: u16, rtp_ts: u32, data: Vec<u8> },
    Sync  { video_ts: u32, audio_ts: u32, ntp_ms: u64 },
}

fn gen_ssrc() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
        .unwrap_or(0x42_42_42_42)
}

impl Transport {
    /// Bind to the fixed backseat port on all interfaces (host mode).
    pub async fn bind() -> Result<Self, String> {
        UdpSocket::bind("0.0.0.0:47474")
            .await
            .map(|s| Self {
                socket:    Arc::new(s),
                video_seq: AtomicU32::new(0),
                audio_seq: AtomicU32::new(0),
                ssrc:      gen_ssrc(),
            })
            .map_err(|e| e.to_string())
    }

    /// Bind to an OS-assigned ephemeral port (viewer mode).
    pub async fn bind_ephemeral() -> Result<Self, String> {
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map(|s| Self {
                socket:    Arc::new(s),
                video_seq: AtomicU32::new(0),
                audio_seq: AtomicU32::new(0),
                ssrc:      gen_ssrc(),
            })
            .map_err(|e| e.to_string())
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
    /// `rtp_ts` is the 90 kHz presentation timestamp from the VP8 encoder.
    /// The RTP marker bit is set on the last fragment of each frame.
    pub async fn send_video(
        &self, to: SocketAddr, rtp_ts: u32, data: &[u8], keyframe: bool,
    ) -> std::io::Result<()> {
        let chunks: Vec<&[u8]> = data.chunks(CHUNK).collect();
        let total    = chunks.len() as u16;
        // Reserve `total` consecutive sequence numbers atomically.
        let base_seq = self.video_seq.fetch_add(total as u32, Ordering::Relaxed) as u16;
        for (i, chunk) in chunks.iter().enumerate() {
            let seq    = base_seq.wrapping_add(i as u16);
            let marker = i + 1 == chunks.len();
            let mut pkt = Vec::with_capacity(19 + chunk.len());
            pkt.push(PKT_VIDEO);
            // 12-byte RTP header
            pkt.push(0x80); // V=2, P=0, X=0, CC=0
            pkt.push(RTP_PT_VP8 | if marker { 0x80 } else { 0x00 }); // M|PT
            pkt.extend_from_slice(&seq.to_be_bytes());
            pkt.extend_from_slice(&rtp_ts.to_be_bytes());
            pkt.extend_from_slice(&self.ssrc.to_be_bytes());
            // Fragmentation metadata
            pkt.extend_from_slice(&(i as u16).to_be_bytes()); // idx
            pkt.extend_from_slice(&total.to_be_bytes());
            pkt.push(if keyframe && i == 0 { 0x01 } else { 0x00 }); // flags
            pkt.extend_from_slice(chunk);
            self.socket.send_to(&pkt, to).await?;
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
    pub async fn recv(&self, buf: &mut Vec<u8>) -> Option<(SocketAddr, Packet)> {
        buf.resize(65_536, 0);
        let (n, from) = match self.socket.recv_from(buf).await {
            Ok(v)  => v,
            Err(e) => { tracing::warn!("recv_from error: {e}"); return None; }
        };
        let data = &buf[..n];
        if data.is_empty() { return None; }
        tracing::debug!("udp rx {n}B from {from} type=0x{:02x}", data[0]);

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
                Packet::Audio { seq, rtp_ts, data: data[13..].to_vec() }
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

            _ => return None,
        };

        Some((from, pkt))
    }
}

// ── Fragment reassembler ──────────────────────────────────────────────────────

/// Maximum fragments accepted per frame.
const MAX_FRAGS_PER_FRAME: u16 = 1000;

/// Collects fragments for multiple in-flight frames and emits complete frames.
pub struct Reassembler {
    frames: HashMap<u32, PendingFrame>,
}

struct PendingFrame {
    frags:    HashMap<u16, Vec<u8>>,
    total:    u16,
    keyframe: bool,
}

impl Reassembler {
    pub fn new() -> Self {
        Self { frames: HashMap::new() }
    }

    /// Feed a fragment. Returns `Some((frame_data, keyframe))` when complete.
    ///
    /// Frames more than 2 seconds (180 000 ticks at 90 kHz) behind the latest
    /// RTP timestamp are evicted to prevent unbounded memory growth.
    pub fn push(
        &mut self,
        rtp_ts: u32, frag_idx: u16, frag_total: u16, keyframe: bool, data: Vec<u8>,
    ) -> Option<(Vec<u8>, bool)> {
        if frag_total == 0 || frag_total > MAX_FRAGS_PER_FRAME || frag_idx >= frag_total {
            return None;
        }
        self.frames.retain(|id, _| rtp_ts.wrapping_sub(*id) <= 180_000);

        let entry = self.frames.entry(rtp_ts).or_insert(PendingFrame {
            frags: HashMap::new(), total: frag_total, keyframe,
        });
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
