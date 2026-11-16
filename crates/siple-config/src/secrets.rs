//! Account passwords in the operating system's credential store (`password_store = "keyring"`).
//!
//! - Entry: service [`KEYRING_SERVICE`], user `account:<id>`.
//! - [`resolve_passwords`] fills `AccountConfig::password` in memory; the loader calls it once
//!   per reload, never per REGISTER (a Keychain access can prompt).
//! - No credential store (headless Linux, Docker): the account fails with
//!   [`UNAVAILABLE_MESSAGE`]. There is never a plaintext fallback.
//! - Errors never contain a password.
//!
//! Backends: `keyring` 4 with its native stores (`apple-native-keyring-store`,
//! `windows-native-keyring-store`, `zbus-secret-service-keyring-store` with pure-Rust crypto).
//! Tests swap the vault with [`set_vault`] and a `keyring_core::mock` store.

use std::sync::{Arc, OnceLock, RwLock};

use keyring_core::api::CredentialStore;

use crate::{AccountConfig, Config, PasswordStore};

/// Service name of every entry: the application's reverse-DNS identifier.
pub const KEYRING_SERVICE: &str = crate::APP_ID;

/// What an account reports when no credential store can be used.
pub const UNAVAILABLE_MESSAGE: &str = "keyring unavailable; set password_store = \"file\"";

/// Entry user name of an account.
pub fn entry_user(account_id: &str) -> String {
    format!("account:{account_id}")
}

/// Why a vault operation failed. Messages never contain a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VaultError {
    /// No usable credential store (none on this system, locked, or access refused).
    #[error("{UNAVAILABLE_MESSAGE}")]
    Unavailable,
    /// The store answered with an error.
    #[error("credential store error: {0}")]
    Failed(String),
}

/// A store of account passwords, addressed by account id.
pub trait Vault: Send + Sync {
    /// The stored password, or `None` when there is no entry.
    fn get(&self, account_id: &str) -> Result<Option<String>, VaultError>;
    /// Stores the password (replacing any existing one).
    fn set(&self, account_id: &str, password: &str) -> Result<(), VaultError>;
    /// Deletes the entry; deleting a missing entry succeeds.
    fn delete(&self, account_id: &str) -> Result<(), VaultError>;
}

/// [`Vault`] over a `keyring-core` credential store.
pub struct KeyringVault {
    store: Arc<CredentialStore>,
}

impl KeyringVault {
    /// Vault over an explicit store (a `keyring_core::mock::Store` in tests).
    pub fn with_store(store: Arc<CredentialStore>) -> Self {
        Self { store }
    }

    /// Vault over the platform's native store, `None` when there is none on this system.
    pub fn system() -> Option<Self> {
        // `keyring::Entry::store_status` installs the native store as the process default once;
        // building it neither prompts nor talks to the service yet.
        if keyring::Entry::store_status().is_err() {
            return None;
        }
        keyring_core::get_default_store().map(Self::with_store)
    }

    fn entry(&self, account_id: &str) -> Result<keyring_core::Entry, VaultError> {
        self.store
            .build(KEYRING_SERVICE, &entry_user(account_id), None)
            .map_err(map_err)
    }
}

fn map_err(e: keyring_core::Error) -> VaultError {
    use keyring_core::Error as E;
    match e {
        E::NoDefaultStore | E::NoStorageAccess(_) | E::PlatformFailure(_) => {
            VaultError::Unavailable
        }
        // Only the Display text, never attached bytes (they could be the secret).
        other => VaultError::Failed(match other {
            E::BadEncoding(_) | E::BadDataFormat(..) => "the stored value is unreadable".into(),
            o => o.to_string(),
        }),
    }
}

impl Vault for KeyringVault {
    fn get(&self, account_id: &str) -> Result<Option<String>, VaultError> {
        match self.entry(account_id)?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(map_err(e)),
        }
    }

    fn set(&self, account_id: &str, password: &str) -> Result<(), VaultError> {
        self.entry(account_id)?
            .set_password(password)
            .map_err(map_err)
    }

    fn delete(&self, account_id: &str) -> Result<(), VaultError> {
        match self.entry(account_id)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_err(e)),
        }
    }
}

/// Stand-in when the system has no credential store: every operation reports it.
struct NoVault;

impl Vault for NoVault {
    fn get(&self, _: &str) -> Result<Option<String>, VaultError> {
        Err(VaultError::Unavailable)
    }
    fn set(&self, _: &str, _: &str) -> Result<(), VaultError> {
        Err(VaultError::Unavailable)
    }
    fn delete(&self, _: &str) -> Result<(), VaultError> {
        Err(VaultError::Unavailable)
    }
}

static OVERRIDE: RwLock<Option<Arc<dyn Vault>>> = RwLock::new(None);
static SYSTEM: OnceLock<Arc<dyn Vault>> = OnceLock::new();

/// Replaces the process-wide vault (`None` restores the system one). For tests and embedders.
pub fn set_vault(vault: Option<Arc<dyn Vault>>) {
    *OVERRIDE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = vault;
}

/// The system vault, created on first use so a config without keyring accounts never touches
/// the credential store.
struct SystemVault;

impl SystemVault {
    fn inner() -> &'static Arc<dyn Vault> {
        SYSTEM.get_or_init(|| match KeyringVault::system() {
            Some(v) => Arc::new(v),
            None => Arc::new(NoVault),
        })
    }
}

impl Vault for SystemVault {
    fn get(&self, account_id: &str) -> Result<Option<String>, VaultError> {
        Self::inner().get(account_id)
    }
    fn set(&self, account_id: &str, password: &str) -> Result<(), VaultError> {
        Self::inner().set(account_id, password)
    }
    fn delete(&self, account_id: &str) -> Result<(), VaultError> {
        Self::inner().delete(account_id)
    }
}

/// The process-wide vault.
pub fn vault() -> Arc<dyn Vault> {
    OVERRIDE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| Arc::new(SystemVault))
}

/// An account whose password could not be read from the credential store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFailure {
    /// Account id.
    pub account: String,
    /// What is wrong (no secret in it).
    pub message: String,
}

impl std::fmt::Display for SecretFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "account {}: {}", self.account, self.message)
    }
}

/// Fills `password` of every enabled `password_store = "keyring"` account from `vault`. An
/// account that cannot be resolved is switched off in memory (`enabled = false`, so it never
/// registers with an empty password) and reported. Disabled accounts are left alone: they
/// would only cause a prompt for nothing. The file is not touched.
pub fn resolve_passwords(config: &mut Config, vault: &dyn Vault) -> Vec<SecretFailure> {
    let mut failures = Vec::new();
    for a in &mut config.accounts {
        if !a.enabled || a.password_store != PasswordStore::Keyring {
            continue;
        }
        match resolve_one(a, vault) {
            Ok(()) => {}
            Err(message) => {
                a.enabled = false;
                failures.push(SecretFailure {
                    account: a.id.clone(),
                    message,
                });
            }
        }
    }
    failures
}

fn resolve_one(a: &mut AccountConfig, vault: &dyn Vault) -> Result<(), String> {
    match vault.get(&a.id) {
        Ok(Some(p)) if !p.is_empty() => {
            a.password = crate::Secret::new(p);
            Ok(())
        }
        Ok(_) => Err(
            "no password in the keyring; set one with account.set_password (or set password_store = \"file\")"
                .into(),
        ),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_vault() -> KeyringVault {
        KeyringVault::with_store(keyring_core::mock::Store::new().unwrap())
    }

    #[test]
    fn round_trip_and_delete() {
        let v = mock_vault();
        assert_eq!(v.get("a").unwrap(), None);
        v.set("a", "pw1").unwrap();
        assert_eq!(v.get("a").unwrap().as_deref(), Some("pw1"));
        v.set("a", "pw2").unwrap();
        assert_eq!(v.get("a").unwrap().as_deref(), Some("pw2"));
        assert_eq!(v.get("b").unwrap(), None);
        v.delete("a").unwrap();
        v.delete("a").unwrap();
        assert_eq!(v.get("a").unwrap(), None);
    }

    #[test]
    fn entry_naming() {
        assert_eq!(KEYRING_SERVICE, "io.zyno.siple");
        assert_eq!(entry_user("work"), "account:work");
    }

    fn cfg(text: &str) -> Config {
        Config::parse(text).unwrap()
    }

    #[test]
    fn resolve_fills_keyring_accounts_only() {
        let v = mock_vault();
        v.set("k", "from-keyring").unwrap();
        let mut c = cfg(
            "[[account]]\nid='k'\npassword_store='keyring'\n[[account]]\nid='f'\npassword='from-file'\n",
        );
        assert!(resolve_passwords(&mut c, &v).is_empty());
        assert_eq!(c.accounts[0].password.expose(), "from-keyring");
        assert_eq!(c.accounts[1].password.expose(), "from-file");
        assert!(c.accounts[0].enabled);
    }

    #[test]
    fn missing_entry_disables_the_account_loudly() {
        let v = mock_vault();
        let mut c = cfg("[[account]]\nid='k'\npassword_store='keyring'\n");
        let f = resolve_passwords(&mut c, &v);
        assert_eq!(f.len(), 1);
        assert!(f[0].message.contains("no password in the keyring"));
        assert!(!c.accounts[0].enabled);
        assert!(c.accounts[0].password.is_empty());
    }

    #[test]
    fn unavailable_keyring_never_falls_back() {
        let mut c = cfg("[[account]]\nid='k'\npassword_store='keyring'\n");
        let f = resolve_passwords(&mut c, &NoVault);
        assert_eq!(f[0].message, UNAVAILABLE_MESSAGE);
        assert!(!c.accounts[0].enabled && c.accounts[0].password.is_empty());
    }

    #[test]
    fn disabled_accounts_are_not_resolved() {
        let mut c = cfg("[[account]]\nid='k'\nenabled=false\npassword_store='keyring'\n");
        assert!(resolve_passwords(&mut c, &NoVault).is_empty());
    }

    #[test]
    fn keyring_requires_an_empty_password() {
        let c = cfg(
            "[[account]]\nid='k'\nusername='1'\ndomain='d'\npassword_store='keyring'\npassword='x'\n",
        );
        let e = c.validate().unwrap_err();
        assert!(e.iter().any(|i| i.path == "account[0].password"));
        assert!(e.iter().all(|i| !i.message.contains("\"x\"")));
        let c = cfg("[[account]]\nid='k'\nusername='1'\ndomain='d'\npassword_store='keyring'\n");
        c.validate().unwrap();
    }
}
