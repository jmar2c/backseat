//! Sticker image reassembly (host) and per-viewer sticker state (both sides).

use std::collections::HashMap;
use std::time::Instant;
use sha2::{Sha256, Digest};
use uuid::Uuid;

use crate::types::NormPoint;

pub const MAX_STICKERS_PER_VIEWER: usize = 10;
pub const MAX_IMAGE_BYTES: usize = 1_048_576; // 1 MiB hard cap

/// A fully assembled, placement-ready sticker on the host.
pub struct HostSticker {
    pub image_bytes: Vec<u8>,   // PNG or JPEG bytes ready for egui texture load
    pub pos:         NormPoint,
    pub size:        NormPoint,
}

/// A live sticker entry on the host egui side (texture loaded, ready to paint).
pub struct HostStickerEntry {
    pub texture: eframe::egui::TextureHandle,
    pub pos:     NormPoint,
    pub size:    NormPoint,
    pub owner:   Uuid,
}

/// Outcome of feeding a chunk or manifest to the reassembler.
pub enum AssembleResult {
    /// Need more chunks; nothing to act on yet.
    Pending,
    /// All chunks received and SHA-256 verified — sticker is ready.
    Complete(HostSticker),
    /// SHA-256 mismatch after all chunks arrived; caller should request a full re-send.
    Corrupt,
    /// Chunk count or size limit exceeded; drop this sticker.
    Rejected,
}

struct PendingSticker {
    chunks:      HashMap<u16, Vec<u8>>,
    total:       u16,
    sha256:      Option<[u8; 32]>,
    pos:         Option<NormPoint>,
    size:        Option<NormPoint>,
    owner:       Uuid,
    arrived_at:  Instant,
}

/// Reassembles incoming image fragments on the host side.
#[derive(Default)]
pub struct StickerReassembler {
    pending: HashMap<u64, PendingSticker>,
}

impl StickerReassembler {
    /// Feed a chunk. Returns `Corrupt`/`Complete` if this was the last needed piece.
    pub fn push_chunk(
        &mut self, sticker_id: u64, total: u16, idx: u16, crc32: u32, data: Vec<u8>,
    ) -> AssembleResult {
        if total == 0 || idx >= total { return AssembleResult::Rejected; }

        // Check CRC32 before storing.
        if crc32fast::hash(&data) != crc32 {
            tracing::warn!("sticker {sticker_id} chunk {idx} CRC32 mismatch — dropping chunk");
            return AssembleResult::Pending; // will be NACK'd on timeout
        }

        let entry = self.pending.entry(sticker_id).or_insert_with(|| PendingSticker {
            chunks:     HashMap::new(),
            total,
            sha256:     None,
            pos:        None,
            size:       None,
            owner:      Uuid::nil(),
            arrived_at: Instant::now(),
        });
        entry.chunks.insert(idx, data);
        self.try_assemble(sticker_id)
    }

    /// Feed the manifest. Returns `Complete` if all chunks were already received.
    pub fn push_manifest(
        &mut self, sticker_id: u64, total_chunks: u16,
        pos_x: f32, pos_y: f32, size_w: f32, size_h: f32,
        sha256: [u8; 32], owner: Uuid,
    ) -> AssembleResult {
        let pos  = NormPoint { x: pos_x, y: pos_y };
        let size = NormPoint { x: size_w, y: size_h };
        let entry = self.pending.entry(sticker_id).or_insert_with(|| PendingSticker {
            chunks:     HashMap::new(),
            total:      total_chunks,
            sha256:     None,
            pos:        None,
            size:       None,
            owner,
            arrived_at: Instant::now(),
        });
        entry.total  = total_chunks;
        entry.sha256 = Some(sha256);
        entry.pos    = Some(pos);
        entry.size   = Some(size);
        entry.owner  = owner;
        self.try_assemble(sticker_id)
    }

    /// Returns (sticker_id, owner, missing_indices) for each sticker pending > 3 s with gaps.
    pub fn collect_nacks(&self) -> Vec<(u64, Uuid, Vec<u16>)> {
        let now = Instant::now();
        self.pending.iter()
            .filter(|(_, p)| now.duration_since(p.arrived_at).as_secs() >= 3)
            .filter_map(|(&id, p)| {
                let missing: Vec<u16> = (0..p.total)
                    .filter(|i| !p.chunks.contains_key(i))
                    .collect();
                if missing.is_empty() { None } else { Some((id, p.owner, missing)) }
            })
            .collect()
    }

    pub fn remove_by_owner(&mut self, viewer_id: Uuid) {
        self.pending.retain(|_, p| p.owner != viewer_id);
    }

    fn try_assemble(&mut self, sticker_id: u64) -> AssembleResult {
        let entry = match self.pending.get(&sticker_id) {
            Some(e) => e,
            None    => return AssembleResult::Pending,
        };
        if entry.chunks.len() != entry.total as usize { return AssembleResult::Pending; }
        let (sha256, pos, size) = match (entry.sha256, entry.pos, entry.size) {
            (Some(h), Some(p), Some(s)) => (h, p, s),
            _ => return AssembleResult::Pending, // manifest not yet received
        };

        // Assemble in order.
        let entry = self.pending.remove(&sticker_id).unwrap();
        let mut parts: Vec<(u16, Vec<u8>)> = entry.chunks.into_iter().collect();
        parts.sort_unstable_by_key(|(i, _)| *i);
        let assembled: Vec<u8> = parts.into_iter().flat_map(|(_, d)| d).collect();

        if assembled.len() > MAX_IMAGE_BYTES {
            tracing::warn!("sticker {sticker_id} exceeds 1 MiB after assembly — rejecting");
            return AssembleResult::Rejected;
        }

        // Verify SHA-256.
        let mut hasher = Sha256::new();
        hasher.update(&assembled);
        let digest: [u8; 32] = hasher.finalize().into();
        if digest != sha256 {
            tracing::warn!("sticker {sticker_id} SHA-256 mismatch — corrupt");
            return AssembleResult::Corrupt;
        }

        AssembleResult::Complete(HostSticker { image_bytes: assembled, pos, size })
    }
}

// ── Viewer-side sticker state ─────────────────────────────────────────────────

/// A sticker as tracked by the viewer that uploaded it.
pub struct ViewerSticker {
    pub sticker_id: u64,
    pub pos:  NormPoint,
    pub size: NormPoint,
}

#[derive(Default)]
pub struct ViewerStickerLayer {
    pub stickers: Vec<ViewerSticker>,
}

impl ViewerStickerLayer {
    pub fn count(&self) -> usize { self.stickers.len() }

    pub fn add(&mut self, sticker: ViewerSticker) {
        self.stickers.push(sticker);
    }

    pub fn remove(&mut self, sticker_id: u64) {
        self.stickers.retain(|s| s.sticker_id != sticker_id);
    }

    pub fn get_mut(&mut self, sticker_id: u64) -> Option<&mut ViewerSticker> {
        self.stickers.iter_mut().find(|s| s.sticker_id == sticker_id)
    }

    /// Returns the sticker_id of the topmost sticker whose rect contains `pt` (normalised).
    pub fn hit_test(&self, pt: NormPoint) -> Option<u64> {
        self.stickers.iter().rev().find_map(|s| {
            let in_x = pt.x >= s.pos.x && pt.x <= s.pos.x + s.size.x;
            let in_y = pt.y >= s.pos.y && pt.y <= s.pos.y + s.size.y;
            (in_x && in_y).then_some(s.sticker_id)
        })
    }
}
