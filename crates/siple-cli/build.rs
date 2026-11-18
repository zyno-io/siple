//! Embeds an `Info.plist` into the macOS `store` build of `siple-cli`.
//!
//! The Mac App Store sandbox traps a bare executable that has no bundle identity at launch, so
//! the CLI inside `Siple.app/Contents/MacOS` carries its own plist in `__TEXT,__info_plist`
//! (identifier `io.zyno.siple-cli`). Only done for `--features store` on macOS: the plain build
//! (Developer ID, Homebrew, rcodesign) is unchanged.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_STORE");
    println!("cargo:rerun-if-env-changed=SIPLE_BUNDLE_VERSION");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
        || env::var_os("CARGO_FEATURE_STORE").is_none()
    {
        return;
    }
    // Apple wants numeric dotted versions: `0.2.0`, never Cargo's `0.2.0-rc.1`. CFBundleVersion
    // comes from scripts/store-version.sh (package-macos.sh exports it), matching Siple.app's.
    let short = format!(
        "{}.{}.{}",
        env::var("CARGO_PKG_VERSION_MAJOR").expect("CARGO_PKG_VERSION_MAJOR"),
        env::var("CARGO_PKG_VERSION_MINOR").expect("CARGO_PKG_VERSION_MINOR"),
        env::var("CARGO_PKG_VERSION_PATCH").expect("CARGO_PKG_VERSION_PATCH"),
    );
    let build = env::var("SIPLE_BUNDLE_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| short.clone());
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>io.zyno.siple-cli</string>
	<key>CFBundleName</key>
	<string>siple-cli</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleShortVersionString</key>
	<string>{short}</string>
	<key>CFBundleVersion</key>
	<string>{build}</string>
</dict>
</plist>
"#
    );
    let path = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("siple-cli-Info.plist");
    fs::write(&path, plist).expect("write Info.plist");
    println!(
        "cargo:rustc-link-arg-bin=siple-cli=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        path.display()
    );
}
