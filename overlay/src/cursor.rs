//! Per-viewer cursor positions tracked on the host side.

use crate::types::{NormPoint, UserId, UserInfo};
use std::collections::HashMap;
use uuid::Uuid;

/// Holds the registered metadata and last-known position for every connected viewer.
#[derive(Default)]
pub struct CursorState {
    pub users:     HashMap<Uuid, UserInfo>,
    pub positions: HashMap<Uuid, NormPoint>,
}

impl CursorState {
    /// Register a new viewer. Must be called before `update` so the cursor has colour/name info.
    pub fn add_user(&mut self, user: UserInfo) {
        self.users.insert(user.id.0, user);
    }

    pub fn remove_user(&mut self, id: &UserId) {
        self.users.remove(&id.0);
        self.positions.remove(&id.0);
    }

    /// Record the latest cursor position received from a viewer.
    pub fn update(&mut self, id: UserId, pos: NormPoint) {
        self.positions.insert(id.0, pos);
    }

    /// Iterate over `(position, color_hex, name)` for all viewers that have a known position.
    pub fn iter_visible(&self) -> impl Iterator<Item = (NormPoint, &str, &str)> {
        self.positions.iter().filter_map(|(id, pos)| {
            self.users
                .get(id)
                .map(|u| (*pos, u.color.0.as_str(), u.name.as_str()))
        })
    }
}
