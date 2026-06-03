//! Thin wrapper around `scrap` for grabbing raw screen frames.

use scrap::{Capturer, Display};

pub struct ScreenCapture {
    capturer: Capturer,
    pub width:  usize,
    pub height: usize,
}

impl ScreenCapture {
    /// Open the primary display for capture.  Fails if no display is available
    /// (e.g. running headless) or if `scrap` can't acquire the capture stream.
    pub fn new() -> Result<Self, String> {
        let display  = Display::primary().map_err(|e| e.to_string())?;
        let width    = display.width();
        let height   = display.height();
        let capturer = Capturer::new(display).map_err(|e| e.to_string())?;
        Ok(Self { capturer, width, height })
    }

    /// Try to grab the next frame.  Returns `None` when the OS hasn't produced a
    /// new frame yet (`WouldBlock`) — callers should just skip and try again next tick.
    /// The returned bytes are in BGRA order (as produced by scrap on Linux/Windows).
    pub fn capture(&mut self) -> Option<Vec<u8>> {
        match self.capturer.frame() {
            Ok(f)  => Some(f.to_vec()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
            Err(e) => { tracing::warn!("capture error: {e}"); None }
        }
    }
}
