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
    ///
    /// On Linux, scrap requires a 32-bit (BGRA) X display.  Virtual displays created by
    /// remote-desktop servers (xrdp, VNC) are sometimes 24-bit and will consistently return
    /// `InvalidData`.  Run the host binary in the physical desktop session to avoid this.
    pub fn capture(&mut self) -> Result<Option<Vec<u8>>, std::io::Error> {
        match self.capturer.frame() {
            Ok(f)  => {
                // scrap may include stride padding: actual row bytes = frame.len() / height.
                // De-stride when the buffer is larger than width * height * 4.
                let expected = self.width * self.height * 4;
                if f.len() == expected {
                    Ok(Some(f.to_vec()))
                } else if f.len() >= expected && f.len() % self.height == 0 {
                    let stride = f.len() / self.height;
                    let row_bytes = self.width * 4;
                    let mut out = Vec::with_capacity(expected);
                    for row in 0..self.height {
                        out.extend_from_slice(&f[row * stride..row * stride + row_bytes]);
                    }
                    Ok(Some(out))
                } else {
                    Ok(Some(f.to_vec()))
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}
