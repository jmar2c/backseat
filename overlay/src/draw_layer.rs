//! Accumulates in-progress and completed ink strokes from all connected viewers.

use crate::types::{NormPoint, UserId};
use std::collections::HashMap;
use uuid::Uuid;

pub struct Stroke {
    pub user_id: Uuid,
    pub points:  Vec<NormPoint>,
    /// `true` once `StrokeEnd` has been received; used to distinguish live vs. finished strokes.
    pub done:    bool,
    pub width:   f32,
    pub color:   String,  // hex "#RRGGBB"
    pub alpha:   u8,      // 255 = opaque, lower = semi-transparent (highlighter)
}

/// All strokes, keyed by the stroke UUID assigned by the viewer that started each one.
#[derive(Default)]
pub struct DrawLayer {
    pub strokes: HashMap<Uuid, Stroke>,
}

impl DrawLayer {
    /// Start a new stroke with explicit style; called when `StrokeBegin` is received.
    pub fn begin_stroke(&mut self, user_id: UserId, stroke_id: Uuid, pos: NormPoint, width: f32, color: String, alpha: u8) {
        self.strokes.insert(stroke_id, Stroke {
            user_id: user_id.0,
            points:  vec![pos],
            done:    false,
            width,
            color,
            alpha,
        });
    }

    /// Append a point to an existing stroke. Creates with default style if the stroke is unknown
    /// (e.g. `StrokeBegin` was lost over UDP).
    pub fn add_point(&mut self, user_id: UserId, stroke_id: Uuid, pos: NormPoint) {
        self.strokes
            .entry(stroke_id)
            .or_insert_with(|| Stroke {
                user_id: user_id.0,
                points: Vec::new(),
                done:   false,
                width:  3.0,
                color:  "#e05c5c".into(),
                alpha:  255,
            })
            .points
            .push(pos);
    }

    /// Mark a stroke as finished so renderers can treat it differently if needed.
    pub fn end_stroke(&mut self, stroke_id: Uuid) {
        if let Some(s) = self.strokes.get_mut(&stroke_id) {
            s.done = true;
        }
    }

    pub fn remove_stroke(&mut self, stroke_id: Uuid) {
        self.strokes.remove(&stroke_id);
    }

    pub fn remove_user_strokes(&mut self, user_id: Uuid) {
        self.strokes.retain(|_, s| s.user_id != user_id);
    }

    pub fn clear(&mut self) {
        self.strokes.clear();
    }
}
