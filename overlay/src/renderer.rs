//! Paints cursors and draw strokes onto the egui Painter each frame.

use eframe::egui::{self, Color32, Painter, Pos2, Rect, Stroke, Vec2};
use std::sync::{Arc, Mutex};

use crate::cursor::CursorState;
use crate::draw_layer::DrawLayer;
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

pub fn hex_to_color32_alpha(hex: &str, alpha: u8) -> Color32 {
    let h = hex.trim_start_matches('#');
    if h.len() == 6 {
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
