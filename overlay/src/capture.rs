//! Screen capture via the `screenshots` crate.
//!
//! On Windows this uses the Windows Graphics Capture API (WGC), which works in
//! remote-access sessions (Chrome Remote Desktop, RDP, etc.) unlike the older
//! DXGI OutputDuplication used by `scrap`.  On Linux it uses XCB.

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
        let screen = screens
            .into_iter()
            .find(|s| s.display_info.is_primary)
            .ok_or_else(|| "no primary display found".to_string())?;

        // Probe once so width/height reflect actual pixel dimensions, not logical units.
        let probe  = screen.capture().map_err(|e| format!("initial capture failed: {e}"))?;
        let width  = probe.width()  as usize;
        let height = probe.height() as usize;

        Ok(Self { screen, width, height })
    }

    /// Capture the current frame.  Returns `Ok(Some(bgra))` on success or
    /// `Err` on failure (e.g. display removed, resolution changed mid-session).
    /// `Ok(None)` is never returned — unlike scrap there is no WouldBlock concept.
    ///
    /// Output bytes are in BGRA order as expected by the VP8 encoder.
    pub fn capture(&mut self) -> Result<Option<Vec<u8>>, std::io::Error> {
        self.screen
            .capture()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
            .map(|img| {
                // The screenshots crate stores pixels as RGBA (image::RgbaImage).
                // The encoder expects BGRA — swap R and B in each pixel.
                let mut data = img.into_raw();
                for px in data.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                Some(data)
            })
    }
}
