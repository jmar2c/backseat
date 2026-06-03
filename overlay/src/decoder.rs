use crate::vpx::ffi::*;
use std::mem::MaybeUninit;

/// VP8 decoder wrapping the libvpx FFI.  Runs on its own OS thread.
pub struct Vp8Decoder {
    ctx: vpx_codec_ctx_t,
}

// Only ever used from the dedicated decode thread.
unsafe impl Send for Vp8Decoder {}

impl Vp8Decoder {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let iface = vpx_codec_vp8_dx();
            let mut ctx = MaybeUninit::<vpx_codec_ctx_t>::uninit();
            let err = vpx_codec_dec_init_ver(
                ctx.as_mut_ptr(),
                iface,
                std::ptr::null(),
                0,
                VPX_DECODER_ABI_VERSION as i32,
            );
            if err != vpx_codec_err_t_VPX_CODEC_OK {
                return Err(format!("vpx_codec_dec_init_ver: {err}"));
            }
            Ok(Self { ctx: ctx.assume_init() })
        }
    }

    /// Decode one VP8 frame.  Returns `(width, height, rgba_bytes)` or `None` on error.
    /// The caller does not need to distinguish keyframes — libvpx handles that internally.
    pub fn decode(&mut self, data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        unsafe {
            let err = vpx_codec_decode(
                &mut self.ctx,
                data.as_ptr(),
                data.len() as u32,
                std::ptr::null_mut(),
                0,
            );
            if err != vpx_codec_err_t_VPX_CODEC_OK {
                tracing::warn!("vpx_codec_decode: {err}");
                return None;
            }

            let mut iter: vpx_codec_iter_t = std::ptr::null();
            let img = vpx_codec_get_frame(&mut self.ctx, &mut iter);
            if img.is_null() { return None; }

            let img = &*img;
            let w = img.d_w as usize;
            let h = img.d_h as usize;
            Some((w as u32, h as u32, i420_to_rgba(img, w, h)))
        }
    }
}

impl Drop for Vp8Decoder {
    fn drop(&mut self) {
        unsafe { vpx_codec_destroy(&mut self.ctx); }
    }
}

/// Convert a libvpx I420 image to packed RGBA using BT.601 full-range coefficients.
unsafe fn i420_to_rgba(img: &vpx_image_t, w: usize, h: usize) -> Vec<u8> {
    let y_stride = img.stride[0] as usize;
    let u_stride = img.stride[1] as usize;
    let v_stride = img.stride[2] as usize;
    let y_plane  = img.planes[0];
    let u_plane  = img.planes[1];
    let v_plane  = img.planes[2];

    // Pre-fill alpha to 255; inner loop only writes RGB.
    let mut rgba = vec![255u8; w * h * 4];

    for row in 0..h {
        for col in 0..w {
            let y = *y_plane.add(row * y_stride + col) as i32 - 16;
            // U/V are 2×2 subsampled — index into the half-resolution chroma planes.
            let u = *u_plane.add((row / 2) * u_stride + col / 2) as i32 - 128;
            let v = *v_plane.add((row / 2) * v_stride + col / 2) as i32 - 128;

            let r = (298 * y + 409 * v + 128) >> 8;
            let g = (298 * y - 100 * u - 208 * v + 128) >> 8;
            let b = (298 * y + 516 * u + 128) >> 8;

            let i = (row * w + col) * 4;
            rgba[i]     = r.clamp(0, 255) as u8;
            rgba[i + 1] = g.clamp(0, 255) as u8;
            rgba[i + 2] = b.clamp(0, 255) as u8;
        }
    }

    rgba
}
