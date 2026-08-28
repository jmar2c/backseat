//! Screen capture via the `screenshots` crate.
//!
//! On Windows this uses the Windows Graphics Capture API (WGC), which works in
//! remote-access sessions (Chrome Remote Desktop, RDP, etc.) unlike the older
//! DXGI OutputDuplication this crate replaced.  On Linux it uses XCB.

use screenshots::Screen;

pub struct ScreenCapture {
    screen: Screen,
    pub width:  usize,
    pub height: usize,
}

impl ScreenCapture {
    /// Open the primary display for capture.  Does an initial probe capture to
    /// determine the true pixel dimensions (correct under HiDPI/scaling).
    pub fn new() -> Result<Self, String> {
        let screens = Screen::all()
            .map_err(|e| format!("display enumeration failed: {e}"))?;
        let screen = screens.iter()
            .find(|s| s.display_info.is_primary)
            .or_else(|| screens.first())
            .copied()
            .ok_or_else(|| "no displays found".to_string())?;

        // Probe once so width/height reflect actual pixel dimensions, not logical units.
        let probe  = screen.capture().map_err(|e| format!("initial capture failed: {e}"))?;
        let width  = probe.width()  as usize;
        let height = probe.height() as usize;

        Ok(Self { screen, width, height })
    }

    /// Capture the current frame.  Returns `Ok(Some(rgba))` on success or
    /// `Err` on failure (e.g. display removed).  `Ok(None)` is never returned —
    /// there is no WouldBlock concept here.
    ///
    /// Output bytes are RGBA as produced by the screenshots crate.
    ///
    /// Note: a resolution change mid-session does *not* produce an `Err`.  The
    /// capture succeeds and simply returns a buffer of the new size, so callers
    /// must compare the length against the dimensions they expect rather than
    /// relying on an error to signal the change.
    pub fn capture(&mut self) -> Result<Option<Vec<u8>>, std::io::Error> {
        self.screen
            .capture()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
            .map(|img| Some(img.into_raw()))
    }
}
