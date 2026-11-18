fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        // App icon + version info, embedded in the .exe.
        println!("cargo:rerun-if-changed=assets/icon.ico");
        let mut res = winresource::WindowsResource::new();
        if !cfg!(windows) {
            // Cross-compiling (e.g. from macOS) with mingw-w64.
            res.set_windres_path("x86_64-w64-mingw32-windres");
            res.set_ar_path("x86_64-w64-mingw32-ar");
        }
        res.set_icon("assets/icon.ico")
            .set("ProductName", "ptp-share")
            .set("FileDescription", "ptp-share: peer-to-peer screen sharing")
            .set("OriginalFilename", "ptp-share.exe");
        res.compile().expect("embedding the Windows icon failed");
    }
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
