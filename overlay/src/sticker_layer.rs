//! Sticker image reassembly (host) and per-viewer sticker state (both sides).

use std::collections::HashMap;
use std::time::{Duration, Instant};
use sha2::{Sha256, Digest};
use uuid::Uuid;

use crate::transport::CHUNK;
use crate::types::NormPoint;

pub const MAX_STICKERS_PER_VIEWER: usize = 10;
pub const MAX_IMAGE_BYTES: usize = 1_048_576; // 1 MiB hard cap

/// Most chunks a legitimate sticker can need. Every chunk but the last is full,
/// so a `total` above this cannot describe an image within [`MAX_IMAGE_BYTES`].
/// Checked before allocating, unlike the post-assembly size check.
pub const MAX_IMAGE_CHUNKS: u16 = MAX_IMAGE_BYTES.div_ceil(CHUNK) as u16;

/// How long an incomplete sticker may sit before it is swept. Chunks are NACK'd
/// from 3 s on a 2 s tick, so a live transfer gets ~4 retry rounds before this.
pub const PENDING_TIMEOUT: Duration = Duration::from_secs(10);

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
    ///
    /// `owner` is resolved from the sending socket address by the caller, never
    /// taken from the packet, so a viewer cannot open slots in someone else's name.
    pub fn push_chunk(
        &mut self, sticker_id: u64, total: u16, idx: u16, crc32: u32, data: Vec<u8>,
        owner: Uuid,
    ) -> AssembleResult {
        if total == 0 || idx >= total || total > MAX_IMAGE_CHUNKS {
            return AssembleResult::Rejected;
        }

        // Check CRC32 before storing.
        if crc32fast::hash(&data) != crc32 {
            tracing::warn!("sticker {sticker_id} chunk {idx} CRC32 mismatch — dropping chunk");
            return AssembleResult::Pending; // will be NACK'd on timeout
        }

        match self.pending.get_mut(&sticker_id) {
            // Chunks may only be added to a transfer the same viewer started,
            // otherwise one viewer could poison another's in-flight sticker.
            Some(entry) if entry.owner != owner => AssembleResult::Rejected,
            Some(entry) => {
                entry.chunks.insert(idx, data);
                self.try_assemble(sticker_id)
            }
            None => {
                if self.pending_for_owner(owner) >= MAX_STICKERS_PER_VIEWER {
                    tracing::warn!(
                        "viewer {owner} at pending-sticker limit, dropping sticker {sticker_id}"
                    );
                    return AssembleResult::Rejected;
                }
                let mut chunks = HashMap::new();
                chunks.insert(idx, data);
                self.pending.insert(sticker_id, PendingSticker {
                    chunks,
                    total,
                    sha256:     None,
                    pos:        None,
                    size:       None,
                    owner,
                    arrived_at: Instant::now(),
                });
                self.try_assemble(sticker_id)
            }
        }
    }

    /// Feed the manifest. Returns `Complete` if all chunks were already received.
    pub fn push_manifest(
        &mut self, sticker_id: u64, total_chunks: u16,
        pos_x: f32, pos_y: f32, size_w: f32, size_h: f32,
        sha256: [u8; 32], owner: Uuid,
    ) -> AssembleResult {
        if total_chunks == 0 || total_chunks > MAX_IMAGE_CHUNKS {
            return AssembleResult::Rejected;
        }
        let pos  = NormPoint { x: pos_x, y: pos_y };
        let size = NormPoint { x: size_w, y: size_h };
        match self.pending.get(&sticker_id) {
            Some(e) if e.owner != owner => return AssembleResult::Rejected,
            None if self.pending_for_owner(owner) >= MAX_STICKERS_PER_VIEWER => {
                tracing::warn!(
                    "viewer {owner} at pending-sticker limit, dropping sticker {sticker_id}"
                );
                return AssembleResult::Rejected;
            }
            _ => {}
        }
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

    /// Incomplete transfers this viewer currently holds open.
    pub fn pending_for_owner(&self, viewer_id: Uuid) -> usize {
        self.pending.values().filter(|p| p.owner == viewer_id).count()
    }

    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Drop transfers that have sat incomplete for longer than `timeout`.
    ///
    /// `arrived_at` is deliberately not refreshed as chunks land, so this is an
    /// absolute deadline: a viewer cannot hold a slot open indefinitely by
    /// dribbling one chunk at a time. Returns the evicted entries for logging.
    pub fn sweep_stale(&mut self, timeout: Duration) -> Vec<(u64, Uuid)> {
        let now = Instant::now();
        let stale: Vec<(u64, Uuid)> = self.pending.iter()
            .filter(|(_, p)| now.duration_since(p.arrived_at) >= timeout)
            .map(|(&id, p)| (id, p.owner))
            .collect();
        for (id, _) in &stale {
            self.pending.remove(id);
        }
        stale
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(data: &[u8]) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(data);
        h.finalize().into()
    }

    /// Feed every chunk of `data`, returning the last result.
    fn push_all(r: &mut StickerReassembler, id: u64, data: &[u8], owner: Uuid) -> AssembleResult {
        let chunks: Vec<&[u8]> = data.chunks(CHUNK).collect();
        let total = chunks.len() as u16;
        let mut last = AssembleResult::Pending;
        for (i, c) in chunks.iter().enumerate() {
            last = r.push_chunk(id, total, i as u16, crc32fast::hash(c), c.to_vec(), owner);
        }
        last
    }

    fn manifest(r: &mut StickerReassembler, id: u64, data: &[u8], owner: Uuid) -> AssembleResult {
        let total = data.chunks(CHUNK).count() as u16;
        r.push_manifest(id, total, 0.1, 0.2, 0.3, 0.4, sha(data), owner)
    }

    #[test]
    fn chunks_then_manifest_completes_with_original_bytes() {
        let (mut r, v) = (StickerReassembler::default(), Uuid::new_v4());
        let data = vec![7u8; CHUNK * 3 + 17];
        assert!(matches!(push_all(&mut r, 1, &data, v), AssembleResult::Pending));
        match manifest(&mut r, 1, &data, v) {
            AssembleResult::Complete(s) => {
                assert_eq!(s.image_bytes, data);
                assert_eq!(s.pos.x, 0.1);
                assert_eq!(s.size.y, 0.4);
            }
            _ => panic!("expected Complete"),
        }
        assert_eq!(r.pending_len(), 0, "completed sticker should not stay pending");
    }

    #[test]
    fn manifest_then_chunks_completes() {
        let (mut r, v) = (StickerReassembler::default(), Uuid::new_v4());
        let data = vec![3u8; CHUNK * 2];
        assert!(matches!(manifest(&mut r, 1, &data, v), AssembleResult::Pending));
        assert!(matches!(push_all(&mut r, 1, &data, v), AssembleResult::Complete(_)));
    }

    /// S2 core: a slot opened by chunks alone must carry the real owner, so
    /// disconnect cleanup can reach it. Before the fix the owner was nil and
    /// `remove_by_owner` could never match.
    #[test]
    fn chunk_only_slot_is_attributed_to_its_sender() {
        let (mut r, v) = (StickerReassembler::default(), Uuid::new_v4());
        r.push_chunk(1, 4, 0, crc32fast::hash(b"x"), b"x".to_vec(), v);
        assert_eq!(r.pending_len(), 1);
        assert_eq!(r.pending_for_owner(v), 1);
        r.remove_by_owner(v);
        assert_eq!(r.pending_len(), 0, "disconnect should free chunk-only slots");
    }

    /// S2: the chunk path had no per-viewer limit, so manifests could be skipped
    /// entirely to open unlimited slots.
    #[test]
    fn pending_slots_are_capped_per_viewer_on_the_chunk_path() {
        let (mut r, v) = (StickerReassembler::default(), Uuid::new_v4());
        for id in 0..MAX_STICKERS_PER_VIEWER as u64 {
            assert!(matches!(
                r.push_chunk(id, 4, 0, crc32fast::hash(b"x"), b"x".to_vec(), v),
                AssembleResult::Pending,
            ));
        }
        assert!(matches!(
            r.push_chunk(999, 4, 0, crc32fast::hash(b"x"), b"x".to_vec(), v),
            AssembleResult::Rejected,
        ));
        assert_eq!(r.pending_len(), MAX_STICKERS_PER_VIEWER);
    }

    #[test]
    fn the_cap_is_per_viewer_not_global() {
        let mut r = StickerReassembler::default();
        for id in 0..MAX_STICKERS_PER_VIEWER as u64 {
            r.push_chunk(id, 4, 0, crc32fast::hash(b"x"), b"x".to_vec(), Uuid::new_v4());
        }
        let fresh = Uuid::new_v4();
        assert!(
            matches!(
                r.push_chunk(999, 4, 0, crc32fast::hash(b"x"), b"x".to_vec(), fresh),
                AssembleResult::Pending,
            ),
            "a viewer with no pending stickers should not be blocked by others",
        );
    }

    /// S2: nothing ever evicted stale slots, despite `arrived_at` existing for it.
    #[test]
    fn stale_pending_is_swept() {
        let (mut r, v) = (StickerReassembler::default(), Uuid::new_v4());
        r.push_chunk(1, 4, 0, crc32fast::hash(b"x"), b"x".to_vec(), v);

        assert!(r.sweep_stale(Duration::from_secs(60)).is_empty(), "not stale yet");
        assert_eq!(r.pending_len(), 1);

        let swept = r.sweep_stale(Duration::ZERO);
        assert_eq!(swept, vec![(1, v)]);
        assert_eq!(r.pending_len(), 0);
    }

    /// S2: `total` is a u16, so one slot could claim 65535 × 1200 B ≈ 78 MB.
    /// It must be refused before anything is allocated.
    #[test]
    fn absurd_chunk_count_is_rejected_without_allocating() {
        let (mut r, v) = (StickerReassembler::default(), Uuid::new_v4());
        assert!(matches!(
            r.push_chunk(1, u16::MAX, 0, crc32fast::hash(b"x"), b"x".to_vec(), v),
            AssembleResult::Rejected,
        ));
        assert_eq!(r.pending_len(), 0, "rejected chunk must not open a slot");

        assert!(matches!(
            r.push_manifest(2, u16::MAX, 0.0, 0.0, 0.1, 0.1, [0; 32], v),
            AssembleResult::Rejected,
        ));
        assert_eq!(r.pending_len(), 0);
    }

    /// The ceiling must still admit a legitimate 1 MiB image — every chunk but
    /// the last is full, so `MAX_IMAGE_BYTES / CHUNK` rounded up is exactly right.
    #[test]
    fn a_maximum_size_image_is_still_accepted() {
        let (mut r, v) = (StickerReassembler::default(), Uuid::new_v4());
        assert_eq!(MAX_IMAGE_CHUNKS as usize, MAX_IMAGE_BYTES.div_ceil(CHUNK));
        assert!((MAX_IMAGE_CHUNKS as usize - 1) * CHUNK < MAX_IMAGE_BYTES);
        assert!(matches!(
            r.push_chunk(1, MAX_IMAGE_CHUNKS, 0, crc32fast::hash(b"x"), b"x".to_vec(), v),
            AssembleResult::Pending,
        ));
        assert!(matches!(
            r.push_chunk(2, MAX_IMAGE_CHUNKS + 1, 0, crc32fast::hash(b"x"), b"x".to_vec(), v),
            AssembleResult::Rejected,
        ));
    }

    #[test]
    fn another_viewer_cannot_poison_an_in_flight_transfer() {
        let (mut r, v1, v2) = (StickerReassembler::default(), Uuid::new_v4(), Uuid::new_v4());
        let data = vec![9u8; CHUNK * 2];
        let chunks: Vec<&[u8]> = data.chunks(CHUNK).collect();
        r.push_chunk(1, 2, 0, crc32fast::hash(chunks[0]), chunks[0].to_vec(), v1);

        // v2 tries to supply the remaining chunk, and to claim the manifest.
        assert!(matches!(
            r.push_chunk(1, 2, 1, crc32fast::hash(b"junk"), b"junk".to_vec(), v2),
            AssembleResult::Rejected,
        ));
        assert!(matches!(manifest(&mut r, 1, &data, v2), AssembleResult::Rejected));

        // v1's transfer is untouched and still completes correctly.
        r.push_chunk(1, 2, 1, crc32fast::hash(chunks[1]), chunks[1].to_vec(), v1);
        match manifest(&mut r, 1, &data, v1) {
            AssembleResult::Complete(s) => assert_eq!(s.image_bytes, data),
            _ => panic!("owner's own transfer should complete"),
        }
    }

    #[test]
    fn tampered_bytes_report_corrupt() {
        let (mut r, v) = (StickerReassembler::default(), Uuid::new_v4());
        let data = vec![1u8; 64];
        let total = 1u16;
        let wrong = vec![2u8; 64];
        r.push_chunk(1, total, 0, crc32fast::hash(&wrong), wrong, v);
        // Manifest carries the hash of the *original*, so assembly must fail.
        assert!(matches!(
            r.push_manifest(1, total, 0.0, 0.0, 0.1, 0.1, sha(&data), v),
            AssembleResult::Corrupt,
        ));
    }

    #[test]
    fn crc_mismatch_drops_the_chunk_and_leaves_it_missing() {
        let (mut r, v) = (StickerReassembler::default(), Uuid::new_v4());
        let data = b"goodpart".to_vec();
        r.push_chunk(1, 2, 0, crc32fast::hash(&data[..4]), data[..4].to_vec(), v);
        assert!(matches!(
            r.push_chunk(1, 2, 1, 0xdead_beef, b"junk".to_vec(), v),
            AssembleResult::Pending,
        ));
        // The bad chunk was not stored, so the transfer is still short a piece
        // even once the manifest lands.
        assert!(
            matches!(
                r.push_manifest(1, 2, 0.0, 0.0, 0.1, 0.1, sha(&data), v),
                AssembleResult::Pending,
            ),
            "a CRC-failed chunk must not count towards completion",
        );
        // Supplying it correctly finishes the transfer.
        assert!(matches!(
            r.push_chunk(1, 2, 1, crc32fast::hash(&data[4..]), data[4..].to_vec(), v),
            AssembleResult::Complete(_),
        ));
    }

    #[test]
    fn zero_total_and_out_of_range_index_are_rejected() {
        let (mut r, v) = (StickerReassembler::default(), Uuid::new_v4());
        for (total, idx) in [(0u16, 0u16), (2, 2), (2, 9)] {
            assert!(matches!(
                r.push_chunk(1, total, idx, crc32fast::hash(b"x"), b"x".to_vec(), v),
                AssembleResult::Rejected,
            ));
        }
        assert_eq!(r.pending_len(), 0);
    }
}
