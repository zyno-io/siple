//! The Microsoft Store source: Siple asks Windows' Store service whether an update of its own
//! package is waiting (`StoreContext::GetAppAndOptionalStorePackageUpdatesAsync`). Siple sends
//! nothing itself; Windows contacts Microsoft as it does for the Store's own updates. The API
//! gives no version number, so the result is "an update is available" (and whether it is
//! mandatory). Nothing is downloaded or installed through it.
//!
//! Only a package signed by the Store is asked: a sideloaded or developer-signed MSIX is not
//! (the Store knows nothing of it), see [`SignatureKind`].

use async_trait::async_trait;

use super::checker::FetchError;

/// How the running package is signed (`Package::SignatureKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureKind {
    /// Signed by the Microsoft Store: updates come from the Store.
    Store,
    /// Developer or test certificate (sideload).
    Developer,
    /// Enterprise line-of-business certificate.
    Enterprise,
    /// OS component.
    System,
    /// Unsigned, or the kind is not known.
    Other,
}

/// What the Store knows about updates of this package.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreUpdates {
    /// An update of this package is waiting.
    pub available: bool,
    /// The Store marked it mandatory.
    pub mandatory: bool,
}

/// The Store side of the check; replaced in tests.
#[async_trait]
pub trait StoreQuery: Send + Sync {
    /// How the running package is signed. Cheap, and made before any update query.
    fn signature_kind(&self) -> SignatureKind;
    /// The waiting updates whose package family is `package_family`.
    async fn updates(&self, package_family: &str) -> Result<StoreUpdates, FetchError>;
}

/// The query of an app whose host did not opt in to the check: never asks the Store.
pub struct OfflineStoreQuery;

#[async_trait]
impl StoreQuery for OfflineStoreQuery {
    fn signature_kind(&self) -> SignatureKind {
        SignatureKind::Store
    }
    async fn updates(&self, _pfn: &str) -> Result<StoreUpdates, FetchError> {
        Err(FetchError::Network(
            "the update check is not enabled".into(),
        ))
    }
}

#[cfg(all(windows, feature = "store"))]
pub use win::WinStoreQuery;

#[cfg(all(windows, feature = "store"))]
mod win {
    use std::sync::{Arc, Mutex, PoisonError};

    use async_trait::async_trait;
    use windows::ApplicationModel::{Package, PackageSignatureKind};
    use windows::Services::Store::StoreContext;

    use super::{SignatureKind, StoreQuery, StoreUpdates};
    use crate::update::checker::FetchError;

    /// The real query through `Windows.Services.Store`.
    ///
    /// No COM initialisation is done here: `signature_kind` runs on whatever thread starts the app
    /// (the GUI's main thread, where winit later needs `OleInitialize`, which fails on a thread
    /// that was put in the MTA), and the `windows` crate already makes an apartment-agnostic call
    /// work on a thread without COM (`CoIncrementMTAUsage`, a process-wide implicit MTA that
    /// leaves the thread's own apartment alone).
    pub struct WinStoreQuery;

    fn net(e: &windows::core::Error) -> FetchError {
        FetchError::Network(format!("Microsoft Store: 0x{:08X}", e.code().0 as u32))
    }

    /// Cancels the Store operation, once.
    type Canceller = Box<dyn FnOnce() + Send>;

    /// Where the Store operation of one query is, shared between the worker and the waiting future.
    enum Slot {
        /// The worker has not started the operation yet.
        Pending,
        /// The future was dropped (time limit, shutdown): the worker must not start or must cancel.
        Cancelled,
        /// Running; calling it cancels the operation.
        Running(Canceller),
    }

    fn cancelled() -> FetchError {
        FetchError::Network("Microsoft Store: the query was cancelled".into())
    }

    fn query(pfn: &str, slot: &Mutex<Slot>) -> Result<StoreUpdates, FetchError> {
        let is_cancelled = || {
            matches!(
                *slot.lock().unwrap_or_else(PoisonError::into_inner),
                Slot::Cancelled
            )
        };
        if is_cancelled() {
            return Err(cancelled());
        }
        let ctx = StoreContext::GetDefault().map_err(|e| net(&e))?;
        let op = ctx
            .GetAppAndOptionalStorePackageUpdatesAsync()
            .map_err(|e| net(&e))?;
        {
            let mut g = slot.lock().unwrap_or_else(PoisonError::into_inner);
            if matches!(*g, Slot::Cancelled) {
                let _ = op.Cancel();
                return Err(cancelled());
            }
            let cancel_op = op.clone();
            *g = Slot::Running(Box::new(move || {
                let _ = cancel_op.Cancel();
            }));
        }
        let updates = op.join().map_err(|e| net(&e))?;
        let mut out = StoreUpdates::default();
        for i in 0..updates.Size().map_err(|e| net(&e))? {
            let u = updates.GetAt(i).map_err(|e| net(&e))?;
            let family = u
                .Package()
                .and_then(|p| p.Id())
                .and_then(|id| id.FamilyName())
                .map_err(|e| net(&e))?;
            if family.to_string().eq_ignore_ascii_case(pfn) {
                out.available = true;
                out.mandatory |= u.Mandatory().unwrap_or(false);
            }
        }
        Ok(out)
    }

    /// Cancels the Store operation when the waiting future is dropped (the checker's time limit).
    struct CancelOnDrop(Arc<Mutex<Slot>>);

    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            let prev = std::mem::replace(
                &mut *self.0.lock().unwrap_or_else(PoisonError::into_inner),
                Slot::Cancelled,
            );
            if let Slot::Running(cancel) = prev {
                cancel();
            }
        }
    }

    #[async_trait]
    impl StoreQuery for WinStoreQuery {
        fn signature_kind(&self) -> SignatureKind {
            match Package::Current().and_then(|p| p.SignatureKind()) {
                Ok(PackageSignatureKind::Store) => SignatureKind::Store,
                Ok(PackageSignatureKind::Developer) => SignatureKind::Developer,
                Ok(PackageSignatureKind::Enterprise) => SignatureKind::Enterprise,
                Ok(PackageSignatureKind::System) => SignatureKind::System,
                _ => SignatureKind::Other,
            }
        }

        async fn updates(&self, package_family: &str) -> Result<StoreUpdates, FetchError> {
            let slot = Arc::new(Mutex::new(Slot::Pending));
            let _cancel = CancelOnDrop(slot.clone());
            let pfn = package_family.to_string();
            tokio::task::spawn_blocking(move || query(&pfn, &slot))
                .await
                .map_err(|_| FetchError::Network("Microsoft Store: query failed".into()))?
        }
    }
}
