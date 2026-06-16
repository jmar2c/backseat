#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code)]
pub mod ffi {
    include!(concat!(env!("OUT_DIR"), "/vpx_bindings.rs"));

    // Encoder control IDs (vp8cx.h enum vp8e_enc_control_id / vp9e_enc_control_id).
    pub const VP8E_SET_CPUUSED:           ::std::os::raw::c_int = 13; // shared VP8+VP9
    pub const VP9E_SET_NOISE_SENSITIVITY: ::std::os::raw::c_int = 38; // unsigned int
    pub const VP9E_SET_TILE_COLUMNS:      ::std::os::raw::c_int = 33; // int (log2)
    pub const VP9E_SET_ROW_MT:            ::std::os::raw::c_int = 55; // unsigned int
    pub const VP9E_SET_TUNE_CONTENT:      ::std::os::raw::c_int = 43; // int (vp9e_tune_content)
    pub const VP9E_CONTENT_SCREEN:        ::std::os::raw::c_int = 1;

    extern "C" {
        // Underlying variadic function behind the vpx_codec_control() macro.
        pub fn vpx_codec_control_(
            ctx:     *mut vpx_codec_ctx_t,
            ctrl_id: ::std::os::raw::c_int,
            ...
        ) -> vpx_codec_err_t;
    }
}
