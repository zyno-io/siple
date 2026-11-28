# Siple patches to winit 0.30.13

Base: the crates.io 0.30.13 package (upstream commit `e9809ef`). Vendored files: the published
(normalized) `Cargo.toml`, `LICENSE`, `README.md`, `FEATURES.md`, `build.rs`, `src/`, `examples/`,
`tests/` and `.cargo_vcs_info.json`. `docs/` (images only) and `Cargo.lock` are not vendored.
Wired in through `[patch.crates-io]` in the workspace `Cargo.toml` (eframe / egui-winit pull winit).

Every change site is marked with a `// siple:` comment (grep for it).

## 1. No private CoreGraphics Services calls (Mac App Store)
- `src/platform_impl/macos/ffi.rs`: the `CGSMainConnectionID` and `CGSSetWindowBackgroundBlurRadius`
  declarations are removed (and the then-unused `NSInteger` / `AnyObject` imports).
- `src/platform_impl/macos/window_delegate.rs` `set_blur`: now a no-op. `WindowAttributes::with_blur`
  / `Window::set_blur` do nothing on macOS. Siple never requests blur.
- Rationale: App Review rejected Siple 0.2.10 for referencing the non-public
  `_CGSSetWindowBackgroundBlurRadius`. `scripts/verify-macos-mas.sh` checks the binaries for `_CGS*`
  imports.
- Exit: drop this vendor copy once the winit that eframe uses no longer references the symbol.
