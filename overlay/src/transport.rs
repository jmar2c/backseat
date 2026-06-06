//! UDP transport layer: packet framing, fragment reassembly, and STUN discovery.
//!
//! Packet layout:
//! ```text
//! [0x01]                                                  PKT_PUNCH
//! [0x02][frame_id:u32be][idx:u16be][total:u16be][flags:u8][data…]  PKT_VIDEO
//! [0x03][utf-8 json…]                                     PKT_ANNOT
//! [0x04]                                                  PKT_DISCONNECT
//! ```
//! `flags` bit 0 = keyframe.  Frames are chunked to 1 200 bytes to stay well
//! below typical path MTU (~1 500 bytes) and avoid IP fragmentation.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;

const PKT_PUNCH:      u8 = 0x01;
const PKT_VIDEO:      u8 = 0x02;
const PKT_ANNOT:      u8 = 0x03;
const PKT_DISCONNECT: u8 = 0x04;
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
}

/// A decoded, typed packet as returned by [`Transport::recv`].
pub enum Packet {
    Punch,
    VideoFrag { frame_id: u32, frag_idx: u16, frag_total: u16, keyframe: bool, data: Vec<u8> },
    Annot(String),
    Disconnect,
}

impl Transport {
    /// Bind to the fixed backseat port on all interfaces (host mode).
    pub async fn bind() -> Result<Self, String> {
        UdpSocket::bind("0.0.0.0:47474")
            .await
            .map(|s| Self { socket: Arc::new(s) })
            .map_err(|e| e.to_string())
    }

    /// Bind to an OS-assigned ephemeral port (viewer mode).
    /// Allows running host and viewer on the same machine without a port conflict.
    pub async fn bind_ephemeral() -> Result<Self, String> {
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map(|s| Self { socket: Arc::new(s) })
            .map_err(|e| e.to_string())
    }

    /// Query the STUN server to learn the public-facing `IP:port` of this socket.
    /// Returns `None` if the query times out or the response can't be parsed.
    pub async fn public_addr(&self) -> Option<SocketAddr> {
        gather_public_addr(&self.socket).await
    }

    /// Format a socket address as a human-readable room code (e.g. `"203.0.113.4:54321"`).
    pub fn room_code(addr: SocketAddr) -> String {
        addr.to_string()
    }

    /// Parse whatever the viewer typed.
    /// Accepts either a 6-letter signaling code (e.g. `"KXPQMZ"`) or a direct `IP:port`.
    pub fn parse_room_code(s: &str) -> Option<RoomCode> {
        let s = s.trim();
        let upper = s.to_ascii_uppercase();
        if upper.len() == 6 && upper.chars().all(|c| c.is_ascii_uppercase()) {
            return Some(RoomCode::Signaling(upper));
        }
        s.parse::<SocketAddr>().ok().map(RoomCode::Direct)
    }

    /// Send a single-byte NAT punch packet.  Used by both sides to open the UDP hole.
    pub async fn send_punch(&self, to: SocketAddr) -> std::io::Result<()> {
        self.socket.send_to(&[PKT_PUNCH], to).await.map(|_| ())
    }

    /// Fragment `data` into `CHUNK`-byte pieces and send each as a `PKT_VIDEO` datagram.
    pub async fn send_video(
        &self, to: SocketAddr, frame_id: u32, data: &[u8], keyframe: bool,
    ) -> std::io::Result<()> {
        let chunks: Vec<&[u8]> = data.chunks(CHUNK).collect();
        let total = chunks.len() as u16;
        for (i, chunk) in chunks.iter().enumerate() {
            let mut pkt = Vec::with_capacity(10 + chunk.len());
            pkt.push(PKT_VIDEO);
            pkt.extend_from_slice(&frame_id.to_be_bytes());
            pkt.extend_from_slice(&(i as u16).to_be_bytes());
            pkt.extend_from_slice(&total.to_be_bytes());
            // Keyframe flag only set on the first fragment to avoid redundancy.
            pkt.push(if keyframe && i == 0 { 0x01 } else { 0x00 });
            pkt.extend_from_slice(chunk);
            self.socket.send_to(&pkt, to).await?;
        }
        Ok(())
    }

    /// Send a JSON annotation message as a `PKT_ANNOT` datagram.
    pub async fn send_annot(&self, to: SocketAddr, json: &str) -> std::io::Result<()> {
        let mut pkt = vec![PKT_ANNOT];
        pkt.extend_from_slice(json.as_bytes());
        self.socket.send_to(&pkt, to).await.map(|_| ())
    }

    /// Wait for the next incoming datagram and parse it into a [`Packet`].
    /// Returns `None` for packets that are malformed or have an unknown type.
    pub async fn recv(&self, buf: &mut Vec<u8>) -> Option<(SocketAddr, Packet)> {
        buf.resize(65_536, 0);
        let (n, from) = match self.socket.recv_from(buf).await {
            Ok(v) => v,
            Err(e) => { tracing::warn!("recv_from error: {e}"); return None; }
        };
        let data = &buf[..n];
        if data.is_empty() { return None; }
        tracing::debug!("udp rx {n}B from {from} type=0x{:02x}", data[0]);

        let pkt = match data[0] {
            PKT_PUNCH => Packet::Punch,

            PKT_VIDEO if data.len() > 10 => {
                let frame_id   = u32::from_be_bytes([data[1], data[2], data[3], data[4]]);
                let frag_idx   = u16::from_be_bytes([data[5], data[6]]);
                let frag_total = u16::from_be_bytes([data[7], data[8]]);
                let keyframe   = data[9] & 0x01 != 0;
                Packet::VideoFrag { frame_id, frag_idx, frag_total, keyframe, data: data[10..].to_vec() }
            }

            PKT_ANNOT if data.len() > 1 => {
                let s = std::str::from_utf8(&data[1..]).ok()?.to_string();
                Packet::Annot(s)
            }

            PKT_DISCONNECT => Packet::Disconnect,

            _ => return None,
        };

        Some((from, pkt))
    }
}

// ── Fragment reassembler ──────────────────────────────────────────────────────

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

    /// Feed a fragment.  Returns `Some((frame_data, keyframe))` when all fragments
    /// for `frame_id` have arrived and been assembled in order.
    ///
    /// Frames more than 60 sequence numbers behind the latest are evicted to
    /// prevent unbounded memory growth from dropped/reordered packets.
    pub fn push(
        &mut self,
        frame_id: u32, frag_idx: u16, frag_total: u16, keyframe: bool, data: Vec<u8>,
    ) -> Option<(Vec<u8>, bool)> {
        self.frames.retain(|id, _| frame_id.wrapping_sub(*id) <= 60);

        let entry = self.frames.entry(frame_id).or_insert(PendingFrame {
            frags: HashMap::new(), total: frag_total, keyframe,
        });
        entry.frags.insert(frag_idx, data);

        if entry.frags.len() == entry.total as usize {
            let e = self.frames.remove(&frame_id)?;
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

/// Send a minimal STUN Binding Request (RFC 5389) on `socket` and parse the
/// XOR-MAPPED-ADDRESS from the response to learn the public IP:port.
pub async fn gather_public_addr(socket: &UdpSocket) -> Option<SocketAddr> {
    stun_query(socket, "stun.l.google.com:19302").await
}

/// Detect symmetric NAT (or a VPN acting as one) by querying two different STUN
/// servers on the same socket.  If the reported external ports differ, the NAT
/// assigns a new mapping per destination — hole-punching will not work reliably.
/// Returns `Some(warning_message)` when a problem is detected, `None` if clean.
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

    // 20-byte STUN header: type=0x0001 (Binding Request), length=0, magic cookie, random TxID.
    let mut req = [0u8; 20];
    req[0] = 0x00; req[1] = 0x01;
    req[2] = 0x00; req[3] = 0x00;
    req[4] = 0x21; req[5] = 0x12; // magic cookie high bytes
    req[6] = 0xA4; req[7] = 0x42; // magic cookie low bytes
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos()).unwrap_or(0);
    req[8..12].copy_from_slice(&seed.to_be_bytes());
    req[12..16].copy_from_slice(&(seed ^ 0xDEAD_BEEF).to_be_bytes());
    req[16..20].copy_from_slice(&seed.wrapping_add(0x1234_5678).to_be_bytes());

    socket.send_to(&req, stun_addr).await.ok()?;

    let mut buf = [0u8; 512];
    let (n, _) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        socket.recv_from(&mut buf),
    ).await.ok()?.ok()?;

    parse_xor_mapped_address(&buf[..n])
}

/// Walk the STUN attribute list looking for XOR-MAPPED-ADDRESS (type 0x0020) and
/// decode the XOR'd IPv4 address and port.
fn parse_xor_mapped_address(buf: &[u8]) -> Option<SocketAddr> {
    if buf.len() < 20 { return None; }
    if buf[0] != 0x01 || buf[1] != 0x01 { return None; } // must be Binding Success Response
    const MAGIC: u32 = 0x2112_A442;
    let body_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    if buf.len() < 20 + body_len { return None; }
    let mut i = 20;
    while i + 4 <= 20 + body_len {
        let attr_type = u16::from_be_bytes([buf[i],     buf[i + 1]]);
        let attr_len  = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        i += 4;
        if i + attr_len > 20 + body_len { break; } // malformed: attribute overflows declared body
        if attr_type == 0x0020 && attr_len >= 8 && buf[i + 1] == 0x01 {
            let port = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) ^ (MAGIC >> 16) as u16;
            let ip   = u32::from_be_bytes([buf[i+4], buf[i+5], buf[i+6], buf[i+7]]) ^ MAGIC;
            return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port));
        }
        i += (attr_len + 3) & !3; // attributes are 4-byte aligned
    }
    None
}

/// Probe the OS routing table to find the LAN IP used to reach the internet.
/// Connects a UDP socket to a public address (no data is sent) and reads the
/// local address the OS chose — that's the active LAN interface IP.
pub fn discover_lan_ip() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:53").ok()?;
    Some(s.local_addr().ok()?.ip())
}
