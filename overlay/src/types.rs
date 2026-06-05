use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Opaque per-viewer identity derived from a random UUID at join time.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct UserId(pub Uuid);

/// Hex colour string (e.g. `"#e05c5c"`) assigned to a viewer for their cursor and strokes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserColor(pub String);

/// Metadata shown in the host's annotation overlay for a connected viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub id:    UserId,
    pub name:  String,
    pub color: UserColor,
}

/// A position in normalised [0.0, 1.0] screen space so annotations scale to any resolution.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NormPoint { pub x: f32, pub y: f32 }

/// Annotation events sent from viewer → host as JSON inside `PKT_ANNOT` UDP packets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnnotMsg {
    /// Sent once on connect so the host can display the viewer's chosen name.
    Register    { viewer_id: Uuid, name: String },
    CursorMove  { viewer_id: Uuid, pos: NormPoint },
    /// Begins a new stroke; carries the per-stroke style chosen by the viewer.
    StrokeBegin { viewer_id: Uuid, stroke_id: Uuid, pos: NormPoint, width: f32, color: String, alpha: u8 },
    StrokePoint { viewer_id: Uuid, stroke_id: Uuid, pos: NormPoint },
    StrokeEnd   { viewer_id: Uuid, stroke_id: Uuid },
    /// Asks the host to delete a specific stroke (used by the eraser tool).
    EraseStroke { viewer_id: Uuid, stroke_id: Uuid },
    ClearAll,
}
