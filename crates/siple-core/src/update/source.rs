//! Which source answers the update check: chosen at run time from the distribution channel and
//! the build (the cargo feature `store` only removes code, see [`select`]).

use std::sync::Arc;

use serde::Serialize;
use siple_config::channel::{Channel, ChannelKind};

use super::appstore::{LookupFetcher, OfflineLookup};
#[cfg(not(feature = "store"))]
use super::checker::{OfflineFetcher, ReleaseFetcher};
use super::msstore::{OfflineStoreQuery, StoreQuery};

/// Why a build or package does not check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisabledReason {
    /// A direct (GitHub) build running inside a store package: the store owns its updates.
    DirectBuildInPackage,
    /// A store build running outside a store package.
    StoreBuildOutsideStore,
    /// An MSIX that the Microsoft Store did not sign (sideload, developer or enterprise).
    Sideloaded,
    /// A Flatpak: Flatpak installs updates from Flathub; the app makes no check.
    ManagedByFlatpak,
}

impl DisabledReason {
    /// The name used in `disabled_reason`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectBuildInPackage => "direct_build_in_package",
            Self::StoreBuildOutsideStore => "store_build_outside_store",
            Self::Sideloaded => "sideloaded",
            Self::ManagedByFlatpak => "managed_by_flatpak",
        }
    }
}

/// The name used in `status.source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceName {
    /// GitHub releases (direct builds).
    Github,
    /// The Mac App Store lookup.
    AppStore,
    /// The Microsoft Store through Windows.
    MicrosoftStore,
    /// No source.
    None,
}

/// The choice [`select`] makes, before any fetcher is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// GitHub releases.
    Github,
    /// The App Store lookup.
    AppStore,
    /// The Microsoft Store query.
    MsStore,
    /// Nothing to check, and why.
    None(DisabledReason),
}

/// The source for `channel` in a build that is (`store_build`) or is not compiled with the cargo
/// feature `store`.
///
/// | build \ channel | direct | Mac App Store | MSIX | Flatpak |
/// |---|---|---|---|---|
/// | default | GitHub | App Store | none: `direct_build_in_package` | none: `direct_build_in_package` |
/// | `store` | none: `store_build_outside_store` | App Store | Microsoft Store | none: `managed_by_flatpak` |
///
/// A package never installs updates itself, whatever the build.
pub fn select(channel: &Channel, store_build: bool) -> SourceKind {
    match (store_build, channel.kind) {
        (false, ChannelKind::Direct) => SourceKind::Github,
        (false, ChannelKind::Msix | ChannelKind::Flatpak) => {
            SourceKind::None(DisabledReason::DirectBuildInPackage)
        }
        (true, ChannelKind::Flatpak) => SourceKind::None(DisabledReason::ManagedByFlatpak),
        (true, ChannelKind::Direct) => SourceKind::None(DisabledReason::StoreBuildOutsideStore),
        (_, ChannelKind::MacAppStore) => SourceKind::AppStore,
        (true, ChannelKind::Msix) => SourceKind::MsStore,
    }
}

/// A source with the fetcher that talks to it.
#[derive(Clone)]
pub enum UpdateSource {
    /// GitHub releases (not in store builds).
    #[cfg(not(feature = "store"))]
    Github(Arc<dyn ReleaseFetcher>),
    /// The App Store lookup.
    AppStore(Arc<dyn LookupFetcher>),
    /// The Microsoft Store query.
    MsStore(Arc<dyn StoreQuery>),
    /// Nothing is checked.
    None(DisabledReason),
}

impl UpdateSource {
    /// The `status.source` value.
    pub fn name(&self) -> SourceName {
        match self {
            #[cfg(not(feature = "store"))]
            Self::Github(_) => SourceName::Github,
            Self::AppStore(_) => SourceName::AppStore,
            Self::MsStore(_) => SourceName::MicrosoftStore,
            Self::None(_) => SourceName::None,
        }
    }

    /// The source as the user knows it, for messages ("rate limited by GitHub").
    pub fn label(&self) -> &'static str {
        match self {
            #[cfg(not(feature = "store"))]
            Self::Github(_) => "GitHub",
            Self::AppStore(_) => "the App Store",
            Self::MsStore(_) => "the Microsoft Store",
            Self::None(_) => "no source",
        }
    }
}

/// The fetchers a host provides. [`Providers::offline`] never touches the network (tests and
/// embedders); the real ones are installed by `AppOptions::use_network_updates`.
#[derive(Clone)]
pub struct Providers {
    /// GitHub releases.
    #[cfg(not(feature = "store"))]
    pub github: Arc<dyn ReleaseFetcher>,
    /// App Store lookup.
    pub lookup: Arc<dyn LookupFetcher>,
    /// Microsoft Store query.
    pub store_query: Arc<dyn StoreQuery>,
    /// Downloader and installer (not in store builds).
    #[cfg(not(feature = "store"))]
    pub install: super::install::InstallProviders,
}

impl Providers {
    /// Fetchers that never touch the network.
    pub fn offline() -> Self {
        Self {
            #[cfg(not(feature = "store"))]
            github: Arc::new(OfflineFetcher),
            lookup: Arc::new(OfflineLookup),
            store_query: Arc::new(OfflineStoreQuery),
            #[cfg(not(feature = "store"))]
            install: super::install::InstallProviders::offline(),
        }
    }

    /// The source for `channel` and this build, with its fetcher attached.
    pub fn source(&self, channel: &Channel) -> UpdateSource {
        match select(channel, cfg!(feature = "store")) {
            #[cfg(not(feature = "store"))]
            SourceKind::Github => UpdateSource::Github(self.github.clone()),
            // `select` never picks GitHub for a store build.
            #[cfg(feature = "store")]
            SourceKind::Github => UpdateSource::None(DisabledReason::StoreBuildOutsideStore),
            SourceKind::AppStore => UpdateSource::AppStore(self.lookup.clone()),
            SourceKind::MsStore => UpdateSource::MsStore(self.store_query.clone()),
            SourceKind::None(why) => UpdateSource::None(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_follows_build_and_channel() {
        let msix = Channel::msix("Siple_x", None);
        let d = DisabledReason::DirectBuildInPackage;
        let o = DisabledReason::StoreBuildOutsideStore;
        assert_eq!(select(&Channel::DIRECT, false), SourceKind::Github);
        assert_eq!(select(&Channel::MAC_APP_STORE, false), SourceKind::AppStore);
        assert_eq!(select(&msix, false), SourceKind::None(d));
        assert_eq!(select(&Channel::DIRECT, true), SourceKind::None(o));
        assert_eq!(select(&Channel::MAC_APP_STORE, true), SourceKind::AppStore);
        assert_eq!(select(&msix, true), SourceKind::MsStore);
        assert_eq!(select(&Channel::FLATPAK, false), SourceKind::None(d));
        assert_eq!(
            select(&Channel::FLATPAK, true),
            SourceKind::None(DisabledReason::ManagedByFlatpak)
        );
    }

    #[test]
    fn reasons_have_stable_names() {
        assert_eq!(
            DisabledReason::DirectBuildInPackage.as_str(),
            "direct_build_in_package"
        );
        assert_eq!(
            serde_json::to_value(DisabledReason::Sideloaded).unwrap(),
            "sideloaded"
        );
        assert_eq!(
            DisabledReason::ManagedByFlatpak.as_str(),
            "managed_by_flatpak"
        );
        assert_eq!(
            serde_json::to_value(DisabledReason::ManagedByFlatpak).unwrap(),
            "managed_by_flatpak"
        );
        assert_eq!(
            serde_json::to_value(SourceName::MicrosoftStore).unwrap(),
            "microsoft_store"
        );
    }
}
