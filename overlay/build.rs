use std::env;
use std::path::PathBuf;
use std::process::Command;

const OPUS_VERSION: &str = "1.4";
const OPUS_URL: &str = "https://downloads.xiph.org/releases/opus/opus-1.4.tar.gz";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let host      = env::var("HOST").unwrap_or_default();
    let out       = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Detect Linux→Windows cross-compilation (the primary CI path for producing overlay.exe).
    let cross_windows = target_os == "windows" && !host.contains("windows");

    // For Windows cross-compilation, ffmpeg-sys-next (without the build feature) uses FFMPEG_DIR
    // to locate the pre-built FFmpeg. We still need to add x264 and Windows system libs to the
    // link list — ffmpeg-sys-next only adds avcodec/avutil/swscale, not the libx264 dependency.
    if cross_windows {
        let ffmpeg_dir = env::var("FFMPEG_DIR").unwrap_or_else(|_| {
            panic!(
                "FFMPEG_DIR must be set when cross-compiling for Windows.\n\
                 Run scripts/cross-windows.sh instead of cargo directly."
            )
        });
        let ffmpeg_dir = PathBuf::from(&ffmpeg_dir);
        println!("cargo:rustc-link-search={}", ffmpeg_dir.join("lib").display());
        // ffmpeg-sys-next bundles libavcodec.a into its rlib (rustc static-lib bundling).
        // avcodec's libx264.o bridge references x264_* symbols.  cargo:rustc-link-lib
        // for x264 would appear BEFORE rlibs in the final link and get discarded by GNU ld
        // (no references to x264 exist yet when it is scanned at that point).
        // Passing the archive by full path as a rustc-link-arg places it AFTER the rlibs,
        // so GNU ld can resolve avcodec's x264 references via selective extraction.
        // x264's extracted objects in turn reference msvcrt/mingwex CRT symbols; those
        // import libraries must come AFTER x264 so GNU ld can still extract the stubs.
        // x264 and its transitive Windows CRT/system dependencies (ratecontrol.o uses
        // log2/_wfopen/fseeko64, win32thread.o uses InitializeCriticalSectionAndSpinCount,
        // etc.) form a chain that requires --start-group/--end-group so GNU ld rescans
        // until all cross-archive references settle.
        let x264_lib = ffmpeg_dir.join("lib").join("libx264.a");
        println!("cargo:rustc-link-arg=-Wl,--start-group");
        println!("cargo:rustc-link-arg={}", x264_lib.display());
        println!("cargo:rustc-link-arg=-lmsvcrt");
        println!("cargo:rustc-link-arg=-lmingwex");
        println!("cargo:rustc-link-arg=-lmingw32");
        println!("cargo:rustc-link-arg=-lgcc");
        println!("cargo:rustc-link-arg=-lkernel32");
        println!("cargo:rustc-link-arg=-Wl,--end-group");
        // Windows system libs needed by the statically-linked FFmpeg.
        println!("cargo:rustc-link-lib=bcrypt");
        println!("cargo:rustc-link-lib=ws2_32");
        println!("cargo:rustc-link-lib=psapi");
    }

    let opus_includes: Vec<PathBuf> = if cross_windows {
        build_opus_cross_windows(&out)
    } else if target_os == "windows" {
        // Native Windows dev: vcpkg must provide opus:x64-windows-static-md.
        let opus = vcpkg::Config::new()
            .lib_name("opus")
            .probe("opus")
            .unwrap_or_else(|e| panic!(
                "Could not find libopus via vcpkg: {e}\n\
                 Run: vcpkg install opus:x64-windows-static-md && vcpkg integrate install"
            ));
        opus.include_paths
    } else {
        // Linux native: system libopus-dev provides the static lib.
        println!("cargo:rustc-link-lib=static=opus");
        vec![]
    };

    // ── Opus bindings ──────────────────────────────────────────────────────────

    let mut opus = bindgen::Builder::default()
        .header_contents("opus_wrapper.h", "#include <opus/opus.h>\n");
    for path in &opus_includes {
        opus = opus.clang_arg(format!("-I{}", path.display()));
    }
    if cross_windows {
        // Tell clang the target ABI so generated types (size_t, etc.) are correct for Windows.
        opus = opus.clang_arg("--target=x86_64-w64-mingw32");
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

/// Download Opus source and cross-compile it for Windows using the MinGW toolchain.
///
/// Requires `x86_64-w64-mingw32-gcc` in PATH (installed via `apt install mingw-w64`).
/// The compiled library is cached in OUT_DIR between incremental builds.
fn build_opus_cross_windows(out: &PathBuf) -> Vec<PathBuf> {
    let install_dir = out.join("opus-windows");
    let lib_file    = install_dir.join("lib").join("libopus.a");

    if !lib_file.exists() {
        let src_dir = out.join(format!("opus-{}", OPUS_VERSION));
        let tarball = out.join("opus.tar.gz");

        // Download the Opus source tarball.
        if !tarball.exists() {
            let status = Command::new("curl")
                .args(["-sSL", "--fail", OPUS_URL, "-o", tarball.to_str().unwrap()])
                .status()
                .expect("failed to run curl — is curl installed?");
            assert!(status.success(), "curl failed to download {OPUS_URL}");
        }

        // Extract.
        if !src_dir.exists() {
            let status = Command::new("tar")
                .args(["-xzf", tarball.to_str().unwrap(), "-C", out.to_str().unwrap()])
                .status()
                .expect("failed to run tar");
            assert!(status.success(), "tar failed to extract opus tarball");
        }

        // Configure: tell autotools to build for x86_64-w64-mingw32.
        // The MinGW gcc (x86_64-w64-mingw32-gcc) is picked up automatically via --host.
        let status = Command::new(src_dir.join("configure"))
            .args([
                "--host=x86_64-w64-mingw32",
                &format!("--prefix={}", install_dir.display()),
                "--enable-static",
                "--disable-shared",
                "--disable-doc",
                "--disable-extra-programs",
            ])
            .current_dir(&src_dir)
            .status()
            .expect("failed to run opus ./configure");
        assert!(status.success(), "opus ./configure returned non-zero");

        // Compile.
        let jobs = std::thread::available_parallelism()
            .map(|n| n.get().to_string())
            .unwrap_or_else(|_| "4".to_string());
        let status = Command::new("make")
            .args(["-j", &jobs])
            .current_dir(&src_dir)
            .status()
            .expect("failed to run make for opus");
        assert!(status.success(), "opus make returned non-zero");

        // Install to prefix.
        let status = Command::new("make")
            .arg("install")
            .current_dir(&src_dir)
            .status()
            .expect("failed to run make install for opus");
        assert!(status.success(), "opus make install returned non-zero");
    }

    println!("cargo:rustc-link-search={}", install_dir.join("lib").display());
    println!("cargo:rustc-link-lib=static=opus");

    vec![install_dir.join("include")]
}
