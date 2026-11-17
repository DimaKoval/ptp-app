fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "macos" {
        println!("cargo:rerun-if-changed=native/macos/ptp_capture.m");
        println!("cargo:rerun-if-changed=native/macos/ptp_capture.h");
        cc::Build::new()
            .file("native/macos/ptp_capture.m")
            .flag("-fobjc-arc")
            .flag("-fmodules")
            .flag("-mmacosx-version-min=13.0")
            .compile("ptp_capture");
        for fw in [
            "ScreenCaptureKit",
            "VideoToolbox",
            "CoreMedia",
            "CoreVideo",
            "CoreGraphics",
            "CoreFoundation",
            "Foundation",
            "AppKit",
            "AudioToolbox",
        ] {
            println!("cargo:rustc-link-lib=framework={fw}");
        }
    }
}
