use crate::vpx::ffi::*;
use std::mem::MaybeUninit;

/// VP9 encoder wrapping the libvpx FFI.  Lives on its own OS thread because
/// the libvpx context is not `Sync`.
pub struct Vp9Encoder {
    ctx:           vpx_codec_ctx_t,
    cfg:           vpx_codec_enc_cfg_t,  // kept for live bitrate updates
    image:         *mut vpx_image_t,
    pts:           i64,
    pts_per_frame: i64,
}

// SAFETY: only ever used from a single OS thread (the capture thread).
unsafe impl Send for Vp9Encoder {}

impl Vp9Encoder {
    /// Initialise a CBR VP9 encoder for `width × height` frames.
    ///
    /// `kf_frames` controls how often a forced keyframe is emitted (encoder units = frames).
    /// `g_error_resilient` is enabled so the decoder can recover if UDP packets are
    /// lost mid-stream without waiting for the next keyframe.
    /// `g_lag_in_frames = 0` disables lookahead, keeping encoding latency at one frame.
    pub fn new(width: u32, height: u32, bitrate_kbps: u32, fps: u32, kf_frames: u64) -> Result<Self, String> {
        unsafe {
            let iface = vpx_codec_vp9_cx();

            let mut cfg = MaybeUninit::<vpx_codec_enc_cfg_t>::uninit();
            let err = vpx_codec_enc_config_default(iface, cfg.as_mut_ptr(), 0);
            if err != vpx_codec_err_t_VPX_CODEC_OK {
                return Err(format!("vpx_codec_enc_config_default: {err}"));
            }
            let mut cfg = cfg.assume_init();

            cfg.g_w               = width;
            cfg.g_h               = height;
            cfg.g_timebase.num    = 1;
            cfg.g_timebase.den    = 90_000;   // 90 kHz RTP clock
            cfg.rc_target_bitrate = bitrate_kbps;
            cfg.g_threads         = 4;
            cfg.g_error_resilient = VPX_ERROR_RESILIENT_DEFAULT;
            cfg.g_lag_in_frames   = 0;        // real-time; no look-ahead delay
            cfg.rc_end_usage      = vpx_rc_mode_VPX_CBR;
            cfg.kf_mode           = vpx_kf_mode_VPX_KF_AUTO;
            cfg.kf_max_dist       = kf_frames as u32;
            // Smaller RC buffers → encoder can't "save up" bits across seconds,
            // which improves per-frame quality responsiveness for screen content.
            cfg.rc_buf_sz         = 100;  // ms (default 1000)
            cfg.rc_buf_initial_sz = 50;
            cfg.rc_buf_optimal_sz = 100;

            let mut ctx = MaybeUninit::<vpx_codec_ctx_t>::uninit();
            let err = vpx_codec_enc_init_ver(
                ctx.as_mut_ptr(),
                iface,
                &cfg,
                0,
                VPX_ENCODER_ABI_VERSION as i32,
            );
            if err != vpx_codec_err_t_VPX_CODEC_OK {
                return Err(format!("vpx_codec_enc_init_ver: {err}"));
            }
            let mut ctx = ctx.assume_init();

            let image = vpx_img_alloc(
                std::ptr::null_mut(),
                vpx_img_fmt_VPX_IMG_FMT_I420,
                width,
                height,
                1,
            );
            if image.is_null() {
                return Err("vpx_img_alloc returned null".into());
            }

            // VP9 controls for real-time screen content.
            vpx_codec_control_(&mut ctx, VP8E_SET_CPUUSED,           8i32);  // VP9 speed 0-9; 8 = real-time
            vpx_codec_control_(&mut ctx, VP9E_SET_NOISE_SENSITIVITY, 0u32);  // disable denoiser — blurs text
            vpx_codec_control_(&mut ctx, VP9E_SET_TILE_COLUMNS,      6i32);  // 2^6 tile columns for threading
            vpx_codec_control_(&mut ctx, VP9E_SET_ROW_MT,            1u32);  // row-based multi-threading
            vpx_codec_control_(&mut ctx, VP9E_SET_TUNE_CONTENT,      VP9E_CONTENT_SCREEN);

            tracing::debug!("encoder init {width}x{height} {bitrate_kbps}kbps {fps}fps kf_every={kf_frames}");
            let pts_per_frame = (90_000 / fps) as i64;
            Ok(Self { ctx, cfg, image, pts: 0, pts_per_frame })
        }
    }

    /// Update the encoder's target bitrate without restarting it.
    pub fn set_bitrate(&mut self, kbps: u32) {
        unsafe {
            self.cfg.rc_target_bitrate = kbps;
            let err = vpx_codec_enc_config_set(&mut self.ctx, &self.cfg);
            if err != vpx_codec_err_t_VPX_CODEC_OK {
                tracing::warn!("vpx_codec_enc_config_set: {err}");
            } else {
                tracing::debug!("encoder bitrate → {kbps} kbps");
            }
        }
    }

    /// Encode one BGRA frame.
    ///
    /// Returns `(vp9_bitstream, rtp_ts)` where `rtp_ts` is the 90 kHz presentation
    /// timestamp used for this frame — pass it directly to [`Transport::send_video`].
    pub fn encode(&mut self, bgra: &[u8], force_keyframe: bool) -> Option<(Vec<u8>, u32)> {
        unsafe {
            self.fill_i420(bgra);

            let pts_used = self.pts;
            let flags    = if force_keyframe { VPX_EFLAG_FORCE_KF as _ } else { 0 };

            let err = vpx_codec_encode(
                &mut self.ctx,
                self.image,
                pts_used,
                1,
                flags,
                VPX_DL_REALTIME as _,
            );
            self.pts += self.pts_per_frame;

            if err != vpx_codec_err_t_VPX_CODEC_OK {
                tracing::warn!("vpx_codec_encode: {err}");
                return None;
            }

            let mut iter: vpx_codec_iter_t = std::ptr::null();
            let mut out = Vec::<u8>::new();

            loop {
                let pkt = vpx_codec_get_cx_data(&mut self.ctx, &mut iter);
                if pkt.is_null() { break; }
                if (*pkt).kind == vpx_codec_cx_pkt_kind_VPX_CODEC_CX_FRAME_PKT {
                    let buf = (*pkt).data.frame.buf as *const u8;
                    let sz  = (*pkt).data.frame.sz;
                    out.extend_from_slice(std::slice::from_raw_parts(buf, sz));
                }
            }

            if out.is_empty() { None } else { Some((out, pts_used as u32)) }
        }
    }

    /// Convert a BGRA frame into the I420 planar layout expected by libvpx.
    /// Uses BT.601 limited-range coefficients (the standard for SD/desktop video).
    unsafe fn fill_i420(&mut self, bgra: &[u8]) {
        let img = &*self.image;
        let w = img.d_w as usize;
        let h = img.d_h as usize;

        let y_stride = img.stride[0] as usize;
        let u_stride = img.stride[1] as usize;
        let v_stride = img.stride[2] as usize;
        let y_plane  = img.planes[0];
        let u_plane  = img.planes[1];
        let v_plane  = img.planes[2];

        for row in 0..h {
            for col in 0..w {
                let src = (row * w + col) * 4;
                let b = bgra[src]     as i32;
                let g = bgra[src + 1] as i32;
                let r = bgra[src + 2] as i32;

                let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                *y_plane.add(row * y_stride + col) = y.clamp(16, 235) as u8;

                // U and V are subsampled 2×2 — only written for every other row/col.
                if row % 2 == 0 && col % 2 == 0 {
                    let ur = row / 2;
                    let uc = col / 2;
                    let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                    let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                    *u_plane.add(ur * u_stride + uc) = u.clamp(16, 240) as u8;
                    *v_plane.add(ur * v_stride + uc) = v.clamp(16, 240) as u8;
                }
            }
        }
    }
}

impl Drop for Vp9Encoder {
    fn drop(&mut self) {
        unsafe {
            vpx_codec_destroy(&mut self.ctx);
            if !self.image.is_null() {
                vpx_img_free(self.image);
            }
        }
    }
}
