use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rustc-link-lib=vpx");
    println!("cargo:rerun-if-changed=build.rs");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    bindgen::Builder::default()
        .header_contents(
            "wrapper.h",
            "#include <vpx/vpx_encoder.h>\n#include <vpx/vp8cx.h>\n#include <vpx/vpx_decoder.h>\n#include <vpx/vp8dx.h>\n",
        )
        .allowlist_function("vpx_codec_vp8_cx")
        .allowlist_function("vpx_codec_enc_config_default")
        .allowlist_function("vpx_codec_enc_init_ver")
        .allowlist_function("vpx_codec_encode")
        .allowlist_function("vpx_codec_get_cx_data")
        .allowlist_function("vpx_codec_destroy")
        .allowlist_function("vpx_img_alloc")
        .allowlist_function("vpx_img_free")
        .allowlist_function("vpx_codec_vp8_dx")
        .allowlist_function("vpx_codec_dec_init_ver")
        .allowlist_function("vpx_codec_decode")
        .allowlist_function("vpx_codec_get_frame")
        .allowlist_type("vpx_codec_ctx_t")
        .allowlist_type("vpx_codec_enc_cfg_t")
        .allowlist_type("vpx_codec_dec_cfg_t")
        .allowlist_type("vpx_image_t")
        .allowlist_type("vpx_codec_cx_pkt_t")
        .allowlist_type("vpx_codec_cx_pkt_kind")
        .allowlist_type("vpx_img_fmt")
        .allowlist_type("vpx_codec_err_t")
        .allowlist_type("vpx_rc_mode")
        .allowlist_type("vpx_kf_mode")
        .allowlist_type("vpx_enc_frame_flags_t")
        .allowlist_var("VPX_ENCODER_ABI_VERSION")
        .allowlist_var("VPX_DL_REALTIME")
        .allowlist_var("VPX_DL_GOOD_QUALITY")
        .allowlist_var("VPX_EFLAG_FORCE_KF")
        .allowlist_var("VPX_ERROR_RESILIENT_DEFAULT")
        .allowlist_var("VPX_CODEC_OK")
        .allowlist_var("VPX_DECODER_ABI_VERSION")
        .allowlist_var("VPX_CBR")
        .allowlist_var("VPX_KF_AUTO")
        .allowlist_var("VPX_IMG_FMT_I420")
        .allowlist_var("VPX_CODEC_CX_FRAME_PKT")
        .generate()
        .expect("Unable to generate vpx bindings")
        .write_to_file(out.join("vpx_bindings.rs"))
        .expect("Couldn't write vpx_bindings.rs");
}
