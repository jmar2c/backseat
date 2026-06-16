use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    let (vpx_includes, opus_includes): (Vec<PathBuf>, Vec<PathBuf>) = if target_os == "windows" {
        // Requires:
        //   vcpkg install libvpx:x64-windows-static-md opus:x64-windows-static-md
        //   vcpkg integrate install
        let vpx = vcpkg::Config::new()
            .lib_name("vpx")
            .probe("libvpx")
            .unwrap_or_else(|e| panic!(
                "Could not find libvpx via vcpkg: {e}\n\
                 Run: vcpkg install libvpx:x64-windows-static-md && vcpkg integrate install"
            ));
        let opus = vcpkg::Config::new()
            .lib_name("opus")
            .probe("opus")
            .unwrap_or_else(|e| panic!(
                "Could not find libopus via vcpkg: {e}\n\
                 Run: vcpkg install opus:x64-windows-static-md && vcpkg integrate install"
            ));
        (vpx.include_paths, opus.include_paths)
    } else {
        println!("cargo:rustc-link-lib=vpx");
        println!("cargo:rustc-link-lib=opus");
        (vec![], vec![])
    };

    // ── VPX bindings ──────────────────────────────────────────────────────────

    let mut vpx = bindgen::Builder::default()
        .header_contents(
            "vpx_wrapper.h",
            "#include <vpx/vpx_encoder.h>\n#include <vpx/vp8cx.h>\n\
             #include <vpx/vpx_decoder.h>\n#include <vpx/vp8dx.h>\n",
        );
    for path in &vpx_includes {
        vpx = vpx.clang_arg(format!("-I{}", path.display()));
    }
    vpx.allowlist_function("vpx_codec_vp8_cx")
        .allowlist_function("vpx_codec_vp9_cx")
        .allowlist_function("vpx_codec_enc_config_default")
        .allowlist_function("vpx_codec_enc_config_set")
        .allowlist_function("vpx_codec_enc_init_ver")
        .allowlist_function("vpx_codec_encode")
        .allowlist_function("vpx_codec_get_cx_data")
        .allowlist_function("vpx_codec_destroy")
        .allowlist_function("vpx_img_alloc")
        .allowlist_function("vpx_img_free")
        .allowlist_function("vpx_codec_vp8_dx")
        .allowlist_function("vpx_codec_vp9_dx")
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

    // ── Opus bindings ─────────────────────────────────────────────────────────

    let mut opus = bindgen::Builder::default()
        .header_contents("opus_wrapper.h", "#include <opus/opus.h>\n");
    for path in &opus_includes {
        opus = opus.clang_arg(format!("-I{}", path.display()));
    }
    opus.allowlist_function("opus_encoder_create")
        .allowlist_function("opus_encode_float")
        .allowlist_function("opus_encoder_destroy")
        .allowlist_function("opus_decoder_create")
        .allowlist_function("opus_decode_float")
        .allowlist_function("opus_decoder_destroy")
        .allowlist_type("OpusEncoder")
        .allowlist_type("OpusDecoder")
        .allowlist_var("OPUS_APPLICATION_VOIP")
        .allowlist_var("OPUS_APPLICATION_AUDIO")
        .allowlist_var("OPUS_OK")
        .generate()
        .expect("Unable to generate opus bindings")
        .write_to_file(out.join("opus_bindings.rs"))
        .expect("Couldn't write opus_bindings.rs");
}
