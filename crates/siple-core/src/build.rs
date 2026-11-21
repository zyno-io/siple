//! The `build` object reported by `hello` and `status`: which edition this process is.

use serde_json::{Value, json};
use siple_config::channel::Channel;

/// `{channel, sandboxed, store, version}` for `channel`.
///
/// `channel` is `direct`, `mas`, `msix` or `flatpak`; `store` says whether the binary was compiled with the
/// `store` feature (store edition: no GitHub update source, no installer); `version` is the Cargo package version.
pub fn build_info(channel: &Channel) -> Value {
    json!({
        "channel": channel.kind.as_str(),
        "sandboxed": channel.sandboxed,
        "store": cfg!(feature = "store"),
        "version": env!("CARGO_PKG_VERSION"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_info_shape() {
        let v = build_info(&Channel::MAC_APP_STORE);
        assert_eq!(v["channel"], "mas");
        assert_eq!(v["sandboxed"], true);
        assert_eq!(v["store"], cfg!(feature = "store"));
        assert!(v["version"].is_string());
        assert_eq!(build_info(&Channel::DIRECT)["channel"], "direct");
        assert_eq!(build_info(&Channel::msix("a_b", None))["channel"], "msix");
        let f = build_info(&Channel::FLATPAK);
        assert_eq!(f["channel"], "flatpak");
        assert_eq!(f["sandboxed"], true);
    }
}
