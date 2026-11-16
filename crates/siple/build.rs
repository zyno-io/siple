//! Embeds the application manifest (PerMonitorV2 DPI awareness, common controls v6, UTF-8 code
//! page), the icon and version info into `siple.exe`. Windows hosts only; elsewhere this is a
//! no-op (`winresource` is a Windows-only build dependency, so macOS/Linux builds never see it).

fn main() {
    #[cfg(windows)]
    embed_windows_resources();
}

#[cfg(windows)]
fn embed_windows_resources() {
    use std::path::PathBuf;

    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let manifest = root.join("packaging/windows/siple.manifest");
    let icon = root.join("packaging/icons/siple.ico");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed=build.rs");

    // Only when actually targeting Windows (build.rs runs on the host).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_manifest_file(manifest.to_str().unwrap());
    res.set_icon(icon.to_str().unwrap());
    res.set("ProductName", "Siple");
    res.set("FileDescription", "Siple SIP softphone");
    res.set("OriginalFilename", "siple.exe");
    res.set("LegalCopyright", "FSL-1.1-ALv2");
    if let Err(e) = res.compile() {
        // Do not fail the build over cosmetics (e.g. rc.exe missing): print a loud warning.
        println!("cargo:warning=could not embed Windows resources: {e}");
    }
}
