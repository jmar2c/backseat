#![allow(non_upper_case_globals, non_camel_case_types)]

use ffmpeg_sys_next as sys;
use std::ffi::CString;
use std::ptr;

/// H.264 encoder using FFmpeg.  Tries hardware encoders (VAAPI on Linux,
/// NVENC/QSV on Windows) and falls back to libx264 if none are available.
/// Lives on its own OS thread because AVCodecContext is not Sync.
pub struct H264Encoder {
    codec_ctx:            *mut sys::AVCodecContext,
    sws_ctx:              *mut sys::SwsContext,
    /// CPU-side frame: RGBA is scaled into this (YUV420P or NV12).
    sw_frame:             *mut sys::AVFrame,
    /// GPU-side frame used only for VAAPI; null for software encoders.
    hw_frame:             *mut sys::AVFrame,
    packet:               *mut sys::AVPacket,
    pts:                  i64,
    pts_per_frame:        i64,
    /// Kept alive for the lifetime of the VAAPI encoder; null otherwise.
    hw_device_ctx:        *mut sys::AVBufferRef,
    /// Cached SPS+PPS NAL units (Annex B); prepended to any IDR frame that
    /// the encoder emits without parameter sets.  Pre-populated from
    /// codec_ctx extradata for hardware encoders that store SPS/PPS there.
    sps_pps:              Vec<u8>,
    /// Byte-count of the length-prefix field in AVCC frames (1, 2, or 4).
    /// 4 for all hardware encoders in practice; read from AVCC extradata.
    avcc_nal_length_size: u8,
    /// True when the encoder outputs AVCC (length-prefixed) packets instead
    /// of Annex B (start-code) packets.  Detected once from codec extradata.
    avcc_output:          bool,
}

// SAFETY: only ever used from a single OS thread (the capture thread).
unsafe impl Send for H264Encoder {}

impl H264Encoder {
    /// Initialise an H.264 encoder.  Hardware encoders are tried first; the
    /// first one that opens successfully is used.  Falls back to libx264.
    pub fn new(
        width: u32, height: u32, bitrate_kbps: u32, fps: u32, kf_frames: u64,
    ) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        const CANDIDATES: &[&str] = &["h264_vaapi", "h264_nvenc", "h264_qsv", "libx264"];
        #[cfg(target_os = "windows")]
        const CANDIDATES: &[&str] = &["h264_nvenc", "h264_qsv", "h264_amf", "libx264"];
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        const CANDIDATES: &[&str] = &["libx264"];

        for &name in CANDIDATES {
            match unsafe { Self::try_init(name, width, height, bitrate_kbps, fps, kf_frames) } {
                Ok(enc) => {
                    tracing::info!("H264 encoder: {name} {width}x{height} {bitrate_kbps}kbps {fps}fps kf_every={kf_frames}");
                    return Ok(enc);
                }
                Err(e) => tracing::debug!("H264 encoder: {name} unavailable: {e}"),
            }
        }
        Err("no H.264 encoder available (tried h264_vaapi, h264_nvenc, h264_qsv, libx264)".into())
    }

    unsafe fn try_init(
        codec_name: &str,
        width: u32, height: u32,
        bitrate_kbps: u32, fps: u32, kf_frames: u64,
    ) -> Result<Self, String> {
        let name_c = CString::new(codec_name).unwrap();
        let codec   = sys::avcodec_find_encoder_by_name(name_c.as_ptr());
        if codec.is_null() {
            return Err("codec not found".into());
        }

        let is_vaapi = codec_name == "h264_vaapi";

        // ── VAAPI: create hardware device context ──────────────────────────────
        let hw_device_ctx = if is_vaapi {
            let mut ctx: *mut sys::AVBufferRef = ptr::null_mut();
            let ret = sys::av_hwdevice_ctx_create(
                &mut ctx,
                sys::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                ptr::null(),
                ptr::null_mut(),
                0,
            );
            if ret < 0 {
                return Err(format!("VAAPI device create: {ret}"));
            }
            ctx
        } else {
            ptr::null_mut()
        };

        let codec_ctx = sys::avcodec_alloc_context3(codec);
        if codec_ctx.is_null() {
            if !hw_device_ctx.is_null() {
                let mut p = hw_device_ctx;
                sys::av_buffer_unref(&mut p);
            }
            return Err("avcodec_alloc_context3 failed".into());
        }

        (*codec_ctx).width        = width  as i32;
        (*codec_ctx).height       = height as i32;
        (*codec_ctx).time_base    = sys::AVRational { num: 1, den: 90_000 };
        (*codec_ctx).framerate    = sys::AVRational { num: fps as i32, den: 1 };
        (*codec_ctx).bit_rate     = (bitrate_kbps * 1_000) as i64;
        (*codec_ctx).gop_size     = kf_frames as i32;
        (*codec_ctx).max_b_frames = 0;
        // CLOSED_GOP makes every forced keyframe (AV_PICTURE_TYPE_I) a true IDR
        // (NAL type 5) rather than a non-IDR I-slice (NAL type 1).  Without this,
        // the libx264 wrapper only emits IDRs for gop_size-scheduled keyframes;
        // on-demand keyframes come out as non-IDR I-frames that the viewer cannot
        // bootstrap decode from.
        (*codec_ctx).flags |= sys::AV_CODEC_FLAG_CLOSED_GOP as i32;
        (*codec_ctx).pix_fmt      = if is_vaapi {
            sys::AVPixelFormat::AV_PIX_FMT_VAAPI
        } else {
            sys::AVPixelFormat::AV_PIX_FMT_YUV420P
        };

        // ── VAAPI: hw_frames_ctx must be attached before avcodec_open2 ─────────
        if is_vaapi {
            let frames_buf = sys::av_hwframe_ctx_alloc(hw_device_ctx);
            if frames_buf.is_null() {
                sys::avcodec_free_context(&mut (codec_ctx as *mut _));
                let mut p = hw_device_ctx; sys::av_buffer_unref(&mut p);
                return Err("av_hwframe_ctx_alloc failed".into());
            }
            {
                let fctx = (*frames_buf).data as *mut sys::AVHWFramesContext;
                (*fctx).format            = sys::AVPixelFormat::AV_PIX_FMT_VAAPI;
                (*fctx).sw_format         = sys::AVPixelFormat::AV_PIX_FMT_NV12;
                (*fctx).width             = width  as i32;
                (*fctx).height            = height as i32;
                (*fctx).initial_pool_size = 4;
            }
            let ret = sys::av_hwframe_ctx_init(frames_buf);
            if ret < 0 {
                let mut p = frames_buf; sys::av_buffer_unref(&mut p);
                sys::avcodec_free_context(&mut (codec_ctx as *mut _));
                let mut p = hw_device_ctx; sys::av_buffer_unref(&mut p);
                return Err(format!("av_hwframe_ctx_init: {ret}"));
            }
            (*codec_ctx).hw_frames_ctx = sys::av_buffer_ref(frames_buf);
            let mut p = frames_buf; sys::av_buffer_unref(&mut p);
        }

        // ── Codec-specific options via AVDictionary ────────────────────────────
        let mut opts: *mut sys::AVDictionary = ptr::null_mut();
        // Prepend SPS+PPS to every IDR frame so viewers who join mid-stream can decode.
        sys::av_dict_set(&mut opts, b"repeat_headers\0".as_ptr() as _, b"1\0".as_ptr() as _, 0);
        match codec_name {
            "libx264" => {
                sys::av_dict_set(&mut opts, b"preset\0".as_ptr() as _, b"ultrafast\0".as_ptr() as _, 0);
                sys::av_dict_set(&mut opts, b"tune\0".as_ptr() as _,   b"zerolatency\0".as_ptr() as _, 0);
            }
            "h264_nvenc" => {
                sys::av_dict_set(&mut opts, b"preset\0".as_ptr() as _,      b"p4\0".as_ptr() as _, 0);
                sys::av_dict_set(&mut opts, b"rc\0".as_ptr() as _,          b"cbr\0".as_ptr() as _, 0);
                sys::av_dict_set(&mut opts, b"zerolatency\0".as_ptr() as _, b"1\0".as_ptr() as _, 0);
            }
            _ => {}
        }

        let ret = sys::avcodec_open2(codec_ctx, codec, &mut opts);
        sys::av_dict_free(&mut opts);
        if ret < 0 {
            sys::avcodec_free_context(&mut (codec_ctx as *mut _));
            if !hw_device_ctx.is_null() {
                let mut p = hw_device_ctx; sys::av_buffer_unref(&mut p);
            }
            return Err(format!("avcodec_open2: {ret}"));
        }

        // ── CPU-side frame: swscale writes RGBA→YUV420P or RGBA→NV12 here ─────
        let sw_pix_fmt = if is_vaapi {
            sys::AVPixelFormat::AV_PIX_FMT_NV12
        } else {
            sys::AVPixelFormat::AV_PIX_FMT_YUV420P
        };

        let sw_frame = sys::av_frame_alloc();
        if sw_frame.is_null() {
            sys::avcodec_free_context(&mut (codec_ctx as *mut _));
            return Err("av_frame_alloc (sw) failed".into());
        }
        (*sw_frame).width  = width  as i32;
        (*sw_frame).height = height as i32;
        (*sw_frame).format = sw_pix_fmt as i32;
        let ret = sys::av_frame_get_buffer(sw_frame, 0);
        if ret < 0 {
            sys::av_frame_free(&mut (sw_frame as *mut _));
            sys::avcodec_free_context(&mut (codec_ctx as *mut _));
            return Err(format!("av_frame_get_buffer (sw): {ret}"));
        }

        // ── GPU frame (VAAPI only) ─────────────────────────────────────────────
        let hw_frame = if is_vaapi {
            let f = sys::av_frame_alloc();
            if f.is_null() {
                sys::av_frame_free(&mut (sw_frame as *mut _));
                sys::avcodec_free_context(&mut (codec_ctx as *mut _));
                return Err("av_frame_alloc (hw) failed".into());
            }
            let ret = sys::av_hwframe_get_buffer((*codec_ctx).hw_frames_ctx, f, 0);
            if ret < 0 {
                sys::av_frame_free(&mut (f as *mut _));
                sys::av_frame_free(&mut (sw_frame as *mut _));
                sys::avcodec_free_context(&mut (codec_ctx as *mut _));
                return Err(format!("av_hwframe_get_buffer: {ret}"));
            }
            f
        } else {
            ptr::null_mut()
        };

        // ── swscale context: RGBA → YUV420P / NV12 ────────────────────────────
        let sws_ctx = sys::sws_getContext(
            width  as i32, height as i32, sys::AVPixelFormat::AV_PIX_FMT_RGBA,
            width  as i32, height as i32, sw_pix_fmt,
            sys::SWS_FAST_BILINEAR as i32,
            ptr::null_mut(), ptr::null_mut(), ptr::null(),
        );
        if sws_ctx.is_null() {
            if !hw_frame.is_null() { sys::av_frame_free(&mut (hw_frame as *mut _)); }
            sys::av_frame_free(&mut (sw_frame as *mut _));
            sys::avcodec_free_context(&mut (codec_ctx as *mut _));
            if !hw_device_ctx.is_null() {
                let mut p = hw_device_ctx; sys::av_buffer_unref(&mut p);
            }
            return Err("sws_getContext failed".into());
        }

        // Some hardware encoders (NVENC, QSV, AMF on Windows) store SPS/PPS in
        // extradata in AVCC format and emit AVCC-framed packets.  Detect the
        // output format once here so we never have to guess per-packet.
        // AVCC extradata starts with configurationVersion=1 (byte 0x01); Annex B
        // extradata starts with a start code (0x00 0x00 ...).
        let (initial_sps_pps, avcc_nal_length_size, avcc_output) =
            if !(*codec_ctx).extradata.is_null() && (*codec_ctx).extradata_size > 0 {
                let extra = std::slice::from_raw_parts(
                    (*codec_ctx).extradata,
                    (*codec_ctx).extradata_size as usize,
                );
                if extra.starts_with(&[0, 0, 0, 1]) || extra.starts_with(&[0, 0, 1]) {
                    // Annex B extradata — encoder writes Annex B packets.
                    (extra.to_vec(), 4u8, false)
                } else if let Some((annexb, nal_len_size)) = parse_avcc_extradata(extra) {
                    tracing::debug!("{codec_name} extradata is AVCC; extracted SPS+PPS ({} bytes, nal_len_size={nal_len_size})", annexb.len());
                    (annexb, nal_len_size, true)
                } else {
                    // Unrecognised extradata format; assume Annex B.
                    (Vec::new(), 4u8, false)
                }
            } else {
                // No extradata → libx264 or similar, always Annex B.
                (Vec::new(), 4u8, false)
            };

        Ok(Self {
            codec_ctx,
            sws_ctx,
            sw_frame,
            hw_frame,
            packet: sys::av_packet_alloc(),
            pts: 0,
            pts_per_frame: (90_000 / fps) as i64,
            hw_device_ctx,
            sps_pps: initial_sps_pps,
            avcc_nal_length_size,
            avcc_output,
        })
    }

    /// Update the encoder's target bitrate.
    pub fn set_bitrate(&mut self, kbps: u32) {
        unsafe { (*self.codec_ctx).bit_rate = (kbps * 1_000) as i64; }
        tracing::debug!("encoder bitrate → {kbps} kbps");
    }

    /// Encode one RGBA frame.
    ///
    /// Returns `(h264_annex_b, rtp_ts)` where `rtp_ts` is the 90 kHz
    /// presentation timestamp — pass it directly to [`Transport::send_video`].
    /// Returns `(h264_annex_b, rtp_ts, is_idr)`.  `is_idr` is true only when
    /// the encoder emitted an actual IDR frame (AV_PKT_FLAG_KEY set on the
    /// packet).  Callers should use this — not the `force_keyframe` input — to
    /// decide whether to mark the transmitted frame as a keyframe.
    pub fn encode(&mut self, rgba: &[u8], force_keyframe: bool) -> Option<(Vec<u8>, u32, bool)> {
        unsafe {
            let w = (*self.sw_frame).width  as usize;
            let h = (*self.sw_frame).height as usize;

            // RGBA → YUV420P / NV12 via swscale (SIMD-optimised)
            let src_data:   [*const u8; 8] = [
                rgba.as_ptr(), ptr::null(), ptr::null(), ptr::null(),
                ptr::null(), ptr::null(), ptr::null(), ptr::null(),
            ];
            let src_stride: [i32; 8] = [w as i32 * 4, 0, 0, 0, 0, 0, 0, 0];
            sys::sws_scale(
                self.sws_ctx,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0, h as i32,
                (*self.sw_frame).data.as_ptr() as *const *mut u8,
                (*self.sw_frame).linesize.as_ptr(),
            );

            let pts_used = self.pts;
            self.pts += self.pts_per_frame;

            (*self.sw_frame).pts       = pts_used;
            (*self.sw_frame).pict_type = if force_keyframe {
                sys::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                sys::AVPictureType::AV_PICTURE_TYPE_NONE
            };

            // For VAAPI: upload CPU NV12 → GPU surface
            let enc_frame = if !self.hw_frame.is_null() {
                (*self.hw_frame).pts       = pts_used;
                (*self.hw_frame).pict_type = (*self.sw_frame).pict_type;
                let ret = sys::av_hwframe_transfer_data(self.hw_frame, self.sw_frame, 0);
                if ret < 0 {
                    tracing::warn!("av_hwframe_transfer_data: {ret}");
                    return None;
                }
                self.hw_frame
            } else {
                self.sw_frame
            };

            let ret = sys::avcodec_send_frame(self.codec_ctx, enc_frame);
            if ret < 0 {
                tracing::warn!("avcodec_send_frame: {ret}");
                return None;
            }

            let mut out = Vec::<u8>::new();
            let mut got_keyframe_pkt = false;
            loop {
                sys::av_packet_unref(self.packet);
                let ret = sys::avcodec_receive_packet(self.codec_ctx, self.packet);
                if ret < 0 { break; }
                if (*self.packet).flags & sys::AV_PKT_FLAG_KEY as i32 != 0 {
                    got_keyframe_pkt = true;
                }
                let data = std::slice::from_raw_parts(
                    (*self.packet).data,
                    (*self.packet).size as usize,
                );
                if self.avcc_output {
                    out.extend_from_slice(&avcc_to_annexb(data, self.avcc_nal_length_size));
                } else {
                    out.extend_from_slice(data);
                }
            }

            // Ensure every IDR frame is self-contained with SPS+PPS so that
            // viewers joining mid-stream can decode without missing the initial
            // parameter sets.
            if got_keyframe_pkt && !out.is_empty() {
                let off = idr_start_offset(&out);
                if off > 0 && off < out.len() {
                    // IDR is preceded by SPS+PPS — refresh the cache.
                    self.sps_pps = out[..off].to_vec();
                } else if off == 0 && !self.sps_pps.is_empty() {
                    // IDR at byte 0 without SPS+PPS — prepend from cache.
                    let mut prefixed = self.sps_pps.clone();
                    prefixed.extend_from_slice(&out);
                    out = prefixed;
                }
                // off == out.len(): idr_start_offset found no IDR NAL; shouldn't
                // reach here when got_keyframe_pkt is true.
            }

            if out.is_empty() { None } else { Some((out, pts_used as u32, got_keyframe_pkt)) }
        }
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        unsafe {
            if !self.hw_frame.is_null()      { sys::av_frame_free(&mut self.hw_frame); }
            if !self.sw_frame.is_null()      { sys::av_frame_free(&mut self.sw_frame); }
            if !self.sws_ctx.is_null()       { sys::sws_freeContext(self.sws_ctx); self.sws_ctx = ptr::null_mut(); }
            if !self.packet.is_null()        { sys::av_packet_free(&mut self.packet); }
            if !self.codec_ctx.is_null()     { sys::avcodec_free_context(&mut self.codec_ctx); }
            if !self.hw_device_ctx.is_null() { sys::av_buffer_unref(&mut self.hw_device_ctx); }
        }
    }
}

/// Parse an AVCDecoderConfigurationRecord (AVCC extradata) into Annex B SPS+PPS bytes.
/// Returns `(annexb_bytes, nal_length_size)`, where `nal_length_size` is the number
/// of bytes used for NALU length prefixes in AVCC-encoded frames (1, 2, or 4).
fn parse_avcc_extradata(data: &[u8]) -> Option<(Vec<u8>, u8)> {
    if data.len() < 7 || data[0] != 1 { return None; }
    let nal_length_size = (data[4] & 0x03) + 1;
    let mut out = Vec::new();
    let num_sps = (data[5] & 0x1F) as usize;
    let mut pos = 6;
    for _ in 0..num_sps {
        if pos + 2 > data.len() { return None; }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + len > data.len() { return None; }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    if pos >= data.len() { return None; }
    let num_pps = data[pos] as usize;
    pos += 1;
    for _ in 0..num_pps {
        if pos + 2 > data.len() { return None; }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + len > data.len() { return None; }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    Some((out, nal_length_size))
}

/// Convert AVCC length-prefixed NALUs into Annex B start-code format.
fn avcc_to_annexb(data: &[u8], nal_length_size: u8) -> Vec<u8> {
    let ls = nal_length_size as usize;
    let mut out = Vec::with_capacity(data.len() + 32);
    let mut i = 0;
    while i + ls <= data.len() {
        let mut nal_len = 0usize;
        for b in &data[i..i + ls] {
            nal_len = (nal_len << 8) | *b as usize;
        }
        i += ls;
        if nal_len == 0 || i + nal_len > data.len() { break; }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[i..i + nal_len]);
        i += nal_len;
    }
    out
}

/// Scan an Annex B bitstream for the first IDR slice NAL (type 5) and return
/// its start-code offset.  Everything before that offset is SPS+PPS header
/// material.  Returns `data.len()` when no IDR is found (non-keyframe output).
fn idr_start_offset(data: &[u8]) -> usize {
    let mut i = 0;
    while i < data.len() {
        let (start, sc_len) = if data.get(i..i + 4) == Some(&[0, 0, 0, 1]) {
            (i, 4usize)
        } else if data.get(i..i + 3) == Some(&[0, 0, 1]) {
            (i, 3usize)
        } else {
            i += 1;
            continue;
        };
        let nalu = start + sc_len;
        if nalu < data.len() && data[nalu] & 0x1F == 5 {
            return start;
        }
        i = nalu + 1;
    }
    data.len()
}
