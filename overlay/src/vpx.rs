#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code)]
pub mod ffi {
    include!(concat!(env!("OUT_DIR"), "/vpx_bindings.rs"));

    // VP8 encoder control IDs (vp8cx.h enum vp8e_enc_control_id).
    pub const VP8E_SET_CPUUSED:           ::std::os::raw::c_int = 13;
    pub const VP8E_SET_NOISE_SENSITIVITY: ::std::os::raw::c_int = 15;
    pub const VP8E_SET_STATIC_THRESHOLD:  ::std::os::raw::c_int = 17;

    extern "C" {
        // Underlying variadic function behind the vpx_codec_control() macro.
        pub fn vpx_codec_control_(
            ctx:     *mut vpx_codec_ctx_t,
            ctrl_id: ::std::os::raw::c_int,
            ...
        ) -> vpx_codec_err_t;
    }
}
