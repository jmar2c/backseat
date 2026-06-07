//! Opus FFI — generated bindings + safe encoder/decoder wrappers.
//!
//! Linux: links against system `libopus` (`libopus-dev`).
//! Windows: vcpkg provides `opus:x64-windows-static-md`.

#[allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/opus_bindings.rs"));
}

#[derive(Clone, Copy)]
pub enum Application {
    Voip,
    Audio,
}

// ── Encoder ───────────────────────────────────────────────────────────────────

pub struct OpusEncoder {
    ptr: *mut ffi::OpusEncoder,
}

unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new(sample_rate: u32, channels: u32, app: Application) -> Result<Self, i32> {
        let app_flag = match app {
            Application::Voip  => ffi::OPUS_APPLICATION_VOIP  as i32,
            Application::Audio => ffi::OPUS_APPLICATION_AUDIO as i32,
        };
        let mut err = 0i32;
        let ptr = unsafe {
            ffi::opus_encoder_create(sample_rate as i32, channels as i32, app_flag, &mut err)
        };
        if ptr.is_null() || err != ffi::OPUS_OK as i32 {
            return Err(err);
        }
        Ok(Self { ptr })
    }

    pub fn encode_float(&mut self, pcm: &[f32], output: &mut [u8]) -> Result<usize, i32> {
        let n = unsafe {
            ffi::opus_encode_float(
                self.ptr,
                pcm.as_ptr(),
                pcm.len() as i32,
                output.as_mut_ptr(),
                output.len() as i32,
            )
        };
        if n < 0 { Err(n) } else { Ok(n as usize) }
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_encoder_destroy(self.ptr); }
    }
}

// ── Decoder ───────────────────────────────────────────────────────────────────

pub struct OpusDecoder {
    ptr: *mut ffi::OpusDecoder,
}

unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new(sample_rate: u32, channels: u32) -> Result<Self, i32> {
        let mut err = 0i32;
        let ptr = unsafe {
            ffi::opus_decoder_create(sample_rate as i32, channels as i32, &mut err)
        };
        if ptr.is_null() || err != ffi::OPUS_OK as i32 {
            return Err(err);
        }
        Ok(Self { ptr })
    }

    pub fn decode_float(&mut self, input: &[u8], output: &mut [f32]) -> Result<usize, i32> {
        let n = unsafe {
            ffi::opus_decode_float(
                self.ptr,
                input.as_ptr(),
                input.len() as i32,
                output.as_mut_ptr(),
                output.len() as i32,
                0,
            )
        };
        if n < 0 { Err(n) } else { Ok(n as usize) }
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_decoder_destroy(self.ptr); }
    }
}
