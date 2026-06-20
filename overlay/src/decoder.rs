#![allow(non_upper_case_globals, non_camel_case_types)]

use ffmpeg_sys_next as sys;
use std::ptr;

/// H.264 decoder using FFmpeg.  Runs on its own OS thread.
pub struct H264Decoder {
    codec_ctx: *mut sys::AVCodecContext,
    /// Lazily initialised swscale context; rebuilt when frame dimensions change.
    sws_ctx:   *mut sys::SwsContext,
    packet:    *mut sys::AVPacket,
    frame:     *mut sys::AVFrame,
    sws_w:     u32,
    sws_h:     u32,
}

// Only ever used from the dedicated decode thread.
unsafe impl Send for H264Decoder {}

impl H264Decoder {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let codec = sys::avcodec_find_decoder(sys::AVCodecID::AV_CODEC_ID_H264);
            if codec.is_null() {
                return Err("H264 decoder not found".into());
            }

            let codec_ctx = sys::avcodec_alloc_context3(codec);
            if codec_ctx.is_null() {
                return Err("avcodec_alloc_context3 failed".into());
            }

            (*codec_ctx).thread_count = 4;
            (*codec_ctx).thread_type  = sys::FF_THREAD_SLICE as i32;
            // Emit corrupt frames rather than nothing on UDP packet loss so the
            // viewer sees degraded-but-moving video instead of a frozen frame.
            (*codec_ctx).flags |= sys::AV_CODEC_FLAG_OUTPUT_CORRUPT as i32;

            let ret = sys::avcodec_open2(codec_ctx, codec, ptr::null_mut());
            if ret < 0 {
                sys::avcodec_free_context(&mut (codec_ctx as *mut _));
                return Err(format!("avcodec_open2: {ret}"));
            }

            Ok(Self {
                codec_ctx,
                sws_ctx: ptr::null_mut(),
                packet:  sys::av_packet_alloc(),
                frame:   sys::av_frame_alloc(),
                sws_w: 0,
                sws_h: 0,
            })
        }
    }

    /// Decode one H.264 Annex B frame.  Returns `(width, height, rgba_bytes)` or `None`.
    pub fn decode(&mut self, data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        unsafe {
            sys::av_packet_unref(self.packet);
            let ret = sys::av_new_packet(self.packet, data.len() as i32);
            if ret < 0 { return None; }
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.packet).data, data.len());

            let ret = sys::avcodec_send_packet(self.codec_ctx, self.packet);
            if ret < 0 {
                tracing::warn!("avcodec_send_packet: {ret}");
                return None;
            }

            sys::av_frame_unref(self.frame);
            if sys::avcodec_receive_frame(self.codec_ctx, self.frame) < 0 {
                return None;
            }

            let w   = (*self.frame).width  as u32;
            let h   = (*self.frame).height as u32;
            let fmt: sys::AVPixelFormat =
                std::mem::transmute::<i32, sys::AVPixelFormat>((*self.frame).format);

            // Rebuild swscale when dimensions (or format) change
            if self.sws_ctx.is_null() || self.sws_w != w || self.sws_h != h {
                if !self.sws_ctx.is_null() {
                    sys::sws_freeContext(self.sws_ctx);
                }
                self.sws_ctx = sys::sws_getContext(
                    w as i32, h as i32, fmt,
                    w as i32, h as i32, sys::AVPixelFormat::AV_PIX_FMT_RGBA,
                    sys::SWS_FAST_BILINEAR as i32,
                    ptr::null_mut(), ptr::null_mut(), ptr::null(),
                );
                self.sws_w = w;
                self.sws_h = h;
                if w > 0 && h > 0 {
                    tracing::debug!("first decoded frame {w}x{h}");
                }
            }
            if self.sws_ctx.is_null() { return None; }

            let mut rgba = vec![0u8; w as usize * h as usize * 4];
            let dst_data:    [*mut u8; 8] = [
                rgba.as_mut_ptr(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut(),
                ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut(),
            ];
            let dst_linesize:[i32; 8]     = [w as i32 * 4, 0, 0, 0, 0, 0, 0, 0];

            sys::sws_scale(
                self.sws_ctx,
                (*self.frame).data.as_ptr() as *const *const u8,
                (*self.frame).linesize.as_ptr(),
                0, h as i32,
                dst_data.as_ptr() as *const *mut u8,
                dst_linesize.as_ptr(),
            );

            Some((w, h, rgba))
        }
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        unsafe {
            if !self.sws_ctx.is_null()   { sys::sws_freeContext(self.sws_ctx); }
            if !self.frame.is_null()     { sys::av_frame_free(&mut self.frame); }
            if !self.packet.is_null()    { sys::av_packet_free(&mut self.packet); }
            if !self.codec_ctx.is_null() { sys::avcodec_free_context(&mut self.codec_ctx); }
        }
    }
}
