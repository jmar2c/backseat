//! Accumulates in-progress and completed ink strokes from all connected viewers.

use crate::types::{NormPoint, UserId};
use std::collections::HashMap;
use uuid::Uuid;

pub struct Stroke {
    pub user_id: Uuid,
    pub points:  Vec<NormPoint>,
    /// `true` once `StrokeEnd` has been received; used to distinguish live vs. finished strokes.
    pub done:    bool,
}

/// All strokes, keyed by the stroke UUID assigned by the viewer that started each one.
#[derive(Default)]
pub struct DrawLayer {
    pub strokes: HashMap<Uuid, Stroke>,
}

impl DrawLayer {
    /// Append a point to a stroke, creating the stroke entry on first use.
    pub fn add_point(&mut self, user_id: UserId, stroke_id: Uuid, pos: NormPoint) {
        self.strokes
            .entry(stroke_id)
            .or_insert_with(|| Stroke {
                user_id: user_id.0,
                points: Vec::new(),
                done: false,
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

    pub fn clear(&mut self) {
        self.strokes.clear();
    }
}
