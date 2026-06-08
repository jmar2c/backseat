//! Accumulates in-progress and completed ink strokes from all connected viewers.

use crate::types::{NormPoint, UserId};
use std::collections::{HashMap, VecDeque};
use uuid::Uuid;

const MAX_STROKES: usize = 4_096;
const MAX_POINTS_PER_STROKE: usize = 4_096;

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
    insertion_order: VecDeque<Uuid>,
}

impl DrawLayer {
    /// Start a new stroke with explicit style; called when `StrokeBegin` is received.
    pub fn begin_stroke(&mut self, user_id: UserId, stroke_id: Uuid, pos: NormPoint, width: f32, color: String, alpha: u8) {
        if self.strokes.len() >= MAX_STROKES {
            if let Some(oldest) = self.insertion_order.pop_front() {
                self.strokes.remove(&oldest);
            }
        }
        self.strokes.insert(stroke_id, Stroke {
            user_id: user_id.0,
            points:  vec![pos],
            done:    false,
            width,
            color,
            alpha,
        });
        self.insertion_order.push_back(stroke_id);
    }

    /// Append a point to an existing stroke. Creates with default style if the stroke is unknown
    /// (e.g. `StrokeBegin` was lost over UDP).
    pub fn add_point(&mut self, user_id: UserId, stroke_id: Uuid, pos: NormPoint) {
        let stroke = self.strokes
            .entry(stroke_id)
            .or_insert_with(|| Stroke {
                user_id: user_id.0,
                points: Vec::new(),
                done:   false,
                width:  3.0,
                color:  "#e05c5c".into(),
                alpha:  255,
            });
        if stroke.points.len() < MAX_POINTS_PER_STROKE {
            stroke.points.push(pos);
        }
    }

    /// Mark a stroke as finished so renderers can treat it differently if needed.
    pub fn end_stroke(&mut self, stroke_id: Uuid) {
        if let Some(s) = self.strokes.get_mut(&stroke_id) {
            s.done = true;
        }
    }

    pub fn remove_stroke(&mut self, stroke_id: Uuid) {
        self.strokes.remove(&stroke_id);
        self.insertion_order.retain(|id| *id != stroke_id);
    }

    pub fn remove_stroke_if_owned(&mut self, stroke_id: Uuid, user_id: Uuid) {
        if self.strokes.get(&stroke_id).map_or(false, |s| s.user_id == user_id) {
            self.remove_stroke(stroke_id);
        }
    }

    pub fn remove_user_strokes(&mut self, user_id: Uuid) {
        self.strokes.retain(|_, s| s.user_id != user_id);
        self.insertion_order.retain(|id| self.strokes.contains_key(id));
    }

}
