//! Paints cursors, draw strokes, and stickers onto the egui Painter each frame.

use eframe::egui::{self, Color32, Painter, Pos2, Rect, Stroke, Vec2};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::cursor::CursorState;
use crate::draw_layer::DrawLayer;
use crate::sticker_layer::{HostStickerEntry, ViewerStickerLayer};
use crate::types::NormPoint;

fn to_screen(p: NormPoint, screen: Rect) -> Pos2 {
    Pos2::new(
        screen.left() + p.x * screen.width(),
        screen.top() + p.y * screen.height(),
    )
}

pub fn hex_to_color32(hex: &str) -> Color32 {
    hex_to_color32_alpha(hex, 255)
}

/// Parse `rrggbb` or `#rrggbb` into a colour, falling back to white.
///
/// Stroke colours arrive over the wire from viewers, so this runs on
/// attacker-controlled input inside the paint loop and must never panic.
/// `h.len()` counts bytes while `&h[0..2]` needs a char boundary, so the
/// ASCII-hex check is what keeps the slices below sound — without it a
/// multi-byte character such as `"aébcd"` is 6 bytes long and splits
/// mid-codepoint.
pub fn hex_to_color32_alpha(hex: &str, alpha: u8) -> Color32 {
    let h = hex.trim_start_matches('#');
    if h.len() == 6 && h.bytes().all(|c| c.is_ascii_hexdigit()) {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&h[0..2], 16),
            u8::from_str_radix(&h[2..4], 16),
            u8::from_str_radix(&h[4..6], 16),
        ) {
            return Color32::from_rgba_unmultiplied(r, g, b, alpha);
        }
    }
    Color32::WHITE
}

/// Normalise a viewer-supplied colour to canonical `#rrggbb`, or `None` if it
/// is not a valid hex colour.
///
/// Applied at the host's trust boundary in `apply_annot` so malformed or
/// oversized colour strings are rejected on arrival rather than stored and
/// re-parsed on every frame.
pub fn sanitize_hex_color(hex: &str) -> Option<String> {
    let h = hex.trim_start_matches('#');
    (h.len() == 6 && h.bytes().all(|c| c.is_ascii_hexdigit()))
        .then(|| format!("#{}", h.to_ascii_lowercase()))
}

/// Paint assembled stickers on the host's transparent overlay.
pub fn paint_stickers(
    painter: &Painter,
    screen: Rect,
    stickers: &HashMap<u64, HostStickerEntry>,
) {
    for entry in stickers.values() {
        let min = to_screen(entry.pos, screen);
        let max = to_screen(
            NormPoint { x: entry.pos.x + entry.size.x, y: entry.pos.y + entry.size.y },
            screen,
        );
        let rect = Rect::from_min_max(min, max);
        painter.image(
            entry.texture.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

/// Paint the viewer's own stickers; when `selected` is set, draws handles and X button.
/// `hover_norm` is the current pointer position in normalised space, used for hover colours.
pub fn paint_viewer_stickers(
    painter: &Painter,
    screen: Rect,
    stickers: &ViewerStickerLayer,
    textures: &HashMap<u64, egui::TextureHandle>,
    selected: Option<u64>,
    hover_norm: Option<NormPoint>,
) {
    for s in &stickers.stickers {
        let tex = match textures.get(&s.sticker_id) {
            Some(t) => t,
            None    => continue,
        };
        let min = to_screen(s.pos, screen);
        let max = to_screen(
            NormPoint { x: s.pos.x + s.size.x, y: s.pos.y + s.size.y },
            screen,
        );
        let rect = Rect::from_min_max(min, max);
        painter.image(
            tex.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );

        if selected == Some(s.sticker_id) {
            // Hover proximity checks (normalised space).
            let corner = NormPoint { x: s.pos.x + s.size.x, y: s.pos.y + s.size.y };
            let x_pt   = NormPoint { x: s.pos.x + s.size.x, y: s.pos.y };
            let (resize_hovered, x_hovered) = hover_norm.map_or((false, false), |h| {
                let cdx = h.x - corner.x; let cdy = h.y - corner.y;
                let xdx = h.x - x_pt.x;  let xdy = h.y - x_pt.y;
                (cdx * cdx + cdy * cdy < 0.025 * 0.025,
                 xdx * xdx + xdy * xdy < 0.018 * 0.018)
            });

            // Double-stroke selection outline — readable on any background.
            painter.rect_stroke(rect, 0.0, Stroke::new(4.0, Color32::from_black_alpha(140)));
            painter.rect_stroke(rect, 0.0, Stroke::new(2.0, Color32::WHITE));

            // Corner resize handle (bottom-right): blue square.
            let br = rect.right_bottom();
            let handle = egui::Rect::from_center_size(br, Vec2::splat(14.0));
            let resize_color = if resize_hovered {
                Color32::from_rgb(80, 160, 255)   // bright blue on hover
            } else {
                Color32::from_rgb(30, 90, 180)    // dark blue at rest
            };
            painter.rect_filled(handle, 3.0, Color32::from_black_alpha(120));
            painter.rect_filled(handle, 3.0, resize_color);
            painter.rect_stroke(handle, 3.0, Stroke::new(1.5, Color32::WHITE));

            // X delete button (top-right).
            let xc = rect.right_top();
            let x_color = if x_hovered {
                Color32::from_rgb(230, 70, 70)    // bright red on hover
            } else {
                Color32::from_rgb(140, 35, 35)    // dark red at rest
            };
            painter.circle_filled(xc, 10.0, Color32::from_black_alpha(120));
            painter.circle_filled(xc, 9.0, x_color);
            painter.text(
                xc,
                egui::Align2::CENTER_CENTER,
                "×",
                egui::FontId::proportional(14.0),
                Color32::WHITE,
            );
        }
    }
}

pub fn paint(
    painter: &Painter,
    screen: Rect,
    draws: &Arc<Mutex<DrawLayer>>,
    cursors: &Arc<Mutex<CursorState>>,
) {
    // ── Draw strokes ──────────────────────────────────────────────────────────
    {
        let layer = draws.lock().unwrap();
        for stroke in layer.strokes.values() {
            if stroke.points.len() < 2 {
                continue;
            }
            let color = hex_to_color32_alpha(&stroke.color, stroke.alpha);
            let pts: Vec<Pos2> = stroke
                .points
                .iter()
                .map(|p| to_screen(*p, screen))
                .collect();
            painter.add(egui::Shape::line(pts, Stroke::new(stroke.width, color)));
        }
    }

    // ── Cursors ───────────────────────────────────────────────────────────────
    {
        let cs = cursors.lock().unwrap();
        for (pos, color_hex, name) in cs.iter_visible() {
            let color = hex_to_color32(color_hex);
            let screen_pos = to_screen(pos, screen);

            // Dot
            painter.circle_filled(screen_pos, 8.0, color);
            painter.circle_stroke(screen_pos, 8.0, Stroke::new(1.5, Color32::WHITE));

            // Name label
            let label_pos = screen_pos + Vec2::new(12.0, -10.0);
            painter.text(
                label_pos,
                egui::Align2::LEFT_TOP,
                name,
                egui::FontId::proportional(13.0),
                Color32::WHITE,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parses_valid_colours_with_and_without_hash() {
        assert_eq!(hex_to_color32("#ff8000"), Color32::from_rgb(255, 128, 0));
        assert_eq!(hex_to_color32("ff8000"),  Color32::from_rgb(255, 128, 0));
        assert_eq!(hex_to_color32("AABBCC"),  Color32::from_rgb(170, 187, 204));
    }

    #[test]
    fn hex_applies_alpha() {
        assert_eq!(
            hex_to_color32_alpha("#ff8000", 128),
            Color32::from_rgba_unmultiplied(255, 128, 0, 128),
        );
    }

    /// A viewer-supplied colour must never panic the host's paint loop.
    /// `"aébcd"` is 5 chars but 6 bytes, so the old byte-length check passed
    /// and `&h[0..2]` split the `é` mid-codepoint.
    #[test]
    fn hex_rejects_multibyte_string_of_six_bytes() {
        assert_eq!(hex_to_color32("aébcd"), Color32::WHITE);
        assert_eq!(hex_to_color32("#aébcd"), Color32::WHITE);
        assert_eq!(hex_to_color32("é" .repeat(3).as_str()), Color32::WHITE);
    }

    #[test]
    fn hex_rejects_wrong_length_and_non_hex() {
        for bad in ["", "#", "fff", "ff80000", "gggggg", "ff 000", "../../x"] {
            assert_eq!(hex_to_color32(bad), Color32::WHITE, "{bad:?} should fall back");
        }
    }

    #[test]
    fn sanitize_normalises_valid_colours() {
        assert_eq!(sanitize_hex_color("#FF8000").as_deref(), Some("#ff8000"));
        assert_eq!(sanitize_hex_color("ff8000").as_deref(),  Some("#ff8000"));
    }

    #[test]
    fn sanitize_rejects_what_the_parser_would_reject() {
        for bad in ["aébcd", "", "fff", "gggggg", &"z".repeat(10_000)] {
            assert_eq!(sanitize_hex_color(bad), None, "{bad:?} should be rejected");
        }
    }

    /// The trust boundary and the painter must agree: sanitising never changes
    /// the colour, and its output is stable under re-sanitising.  Compared
    /// against the parse of the original rather than against `WHITE`, since
    /// white is both a legitimate colour and the parser's failure value.
    #[test]
    fn sanitize_preserves_colour_and_is_idempotent() {
        for good in ["#ff8000", "AABBCC", "000000", "ffffff"] {
            let s = sanitize_hex_color(good).expect("should be accepted");
            assert_eq!(hex_to_color32(&s), hex_to_color32(good), "{good:?} changed colour");
            assert_eq!(sanitize_hex_color(&s).as_deref(), Some(s.as_str()));
        }
    }
}
