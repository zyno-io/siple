//! Classification of account config changes.

use siple_config::AccountConfig;

use crate::types::UpdateOutcome;

/// What a config change requires, before considering active calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Apply in place.
    Hot,
    /// REGISTER again (credentials / lifetime / AOR / register flag changed).
    Reregister,
    /// Rebuild the transport (also covers false -> true of `enabled`).
    Rebuild,
    /// `enabled` true -> false: tear down now. Never deferred by active calls (they are hung up);
    /// a disabled account must not keep registering or answering.
    Disable,
}

/// Fields whose change needs a transport rebuild.
pub fn rebuild_needed(old: &AccountConfig, new: &AccountConfig) -> bool {
    old.enabled != new.enabled
        || old.transport != new.transport
        || old.effective_registrar() != new.effective_registrar()
        || old.outbound_proxy != new.outbound_proxy
        || old.outbound_proxy_route != new.outbound_proxy_route
        || old.local_port != new.local_port
        || old.bind_address != new.bind_address
        || old.public_address != new.public_address
        || old.use_sips_uri != new.use_sips_uri
        || old.tls_verify != new.tls_verify
        || old.tls_ca_file != new.tls_ca_file
        || old.tls_server_name != new.tls_server_name
        || old.tls_fingerprint != new.tls_fingerprint
        || old.tls_cert != new.tls_cert
        || old.tls_key != new.tls_key
}

/// Fields whose change needs a fresh REGISTER (and an unregister of the old AOR when the AOR
/// itself changed).
pub fn reregister_needed(old: &AccountConfig, new: &AccountConfig) -> bool {
    old.expires != new.expires
        || old.password != new.password
        || old.effective_auth_username() != new.effective_auth_username()
        || old.username != new.username
        || old.domain != new.domain
        || old.register != new.register
        || old.rewrite_contact != new.rewrite_contact
}

/// Whether the address of record changed.
pub fn aor_changed(old: &AccountConfig, new: &AccountConfig) -> bool {
    old.username != new.username || old.domain != new.domain
}

/// Strongest requirement of `old -> new`.
pub fn classify(old: &AccountConfig, new: &AccountConfig) -> Need {
    if old.enabled && !new.enabled {
        Need::Disable
    } else if rebuild_needed(old, new) {
        Need::Rebuild
    } else if reregister_needed(old, new) {
        Need::Reregister
    } else {
        Need::Hot
    }
}

/// Final outcome given the account's call state. A rebuild with active calls is deferred unless
/// `reconfig_force`.
pub fn outcome(need: Need, has_calls: bool, reconfig_force: bool) -> UpdateOutcome {
    match need {
        Need::Hot => UpdateOutcome::Hot,
        Need::Reregister => UpdateOutcome::Reregistered,
        Need::Disable => UpdateOutcome::Rebuilt,
        Need::Rebuild if has_calls && !reconfig_force => UpdateOutcome::RebuildDeferred,
        Need::Rebuild => UpdateOutcome::Rebuilt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use siple_config::{DtmfMode, OrInherit, Secret, Transport};

    fn base() -> AccountConfig {
        AccountConfig {
            id: "a".into(),
            username: "1003".into(),
            domain: "pbx.example".into(),
            password: Secret::new("pw"),
            ..Default::default()
        }
    }

    fn with(f: impl FnOnce(&mut AccountConfig)) -> Need {
        let mut n = base();
        f(&mut n);
        classify(&base(), &n)
    }

    #[test]
    fn hot_fields() {
        assert_eq!(with(|_| {}), Need::Hot);
        assert_eq!(with(|c| c.dnd = true), Need::Hot);
        assert_eq!(with(|c| c.auth_foreign = true), Need::Hot);
        assert_eq!(with(|c| c.auto_answer = true), Need::Hot);
        assert_eq!(with(|c| c.sip_trace = true), Need::Hot);
        assert_eq!(with(|c| c.display_name = "X".into()), Need::Hot);
        assert_eq!(with(|c| c.user_agent = "UA/9".into()), Need::Hot);
        assert_eq!(with(|c| c.keepalive_secs = 10), Need::Hot);
        assert_eq!(with(|c| c.keepalive_fail_threshold = 7), Need::Hot);
        assert_eq!(
            with(|c| c.keepalive_method = siple_config::KeepaliveMethod::Options),
            Need::Hot
        );
        assert_eq!(with(|c| c.max_calls = 9), Need::Hot);
        assert_eq!(
            with(|c| c.dtmf_mode = OrInherit::Value(DtmfMode::Info)),
            Need::Hot
        );
        assert_eq!(
            with(|c| {
                c.extra_headers.insert("X-A".into(), "b".into());
            }),
            Need::Hot
        );
    }

    #[test]
    fn reregister_fields() {
        assert_eq!(with(|c| c.expires = 60), Need::Reregister);
        assert_eq!(
            with(|c| c.password = Secret::new("other")),
            Need::Reregister
        );
        assert_eq!(
            with(|c| c.auth_username = "authid".into()),
            Need::Reregister
        );
        assert_eq!(with(|c| c.username = "1004".into()), Need::Reregister);
        assert_eq!(with(|c| c.register = false), Need::Reregister);
        assert_eq!(with(|c| c.rewrite_contact = false), Need::Reregister);
        let mut no_rewrite = base();
        no_rewrite.rewrite_contact = false;
        assert_eq!(classify(&no_rewrite, &base()), Need::Reregister);
        assert!(aor_changed(&base(), &{
            let mut n = base();
            n.username = "x".into();
            n
        }));
        assert!(!aor_changed(&base(), &{
            let mut n = base();
            n.expires = 1;
            n
        }));
    }

    #[test]
    fn rebuild_fields() {
        assert_eq!(with(|c| c.transport = Transport::Tcp), Need::Rebuild);
        assert_eq!(
            with(|c| c.registrar = "other.example".into()),
            Need::Rebuild
        );
        assert_eq!(
            with(|c| c.outbound_proxy = "proxy:5060".into()),
            Need::Rebuild
        );
        assert_eq!(with(|c| c.local_port = 5070), Need::Rebuild);
        assert_eq!(with(|c| c.bind_address = "127.0.0.1".into()), Need::Rebuild);
        assert_eq!(
            with(|c| c.public_address = "203.0.113.1".into()),
            Need::Rebuild
        );
        assert_eq!(with(|c| c.tls_verify = false), Need::Rebuild);
        assert_eq!(with(|c| c.tls_ca_file = "ca.pem".into()), Need::Rebuild);
        assert_eq!(with(|c| c.tls_server_name = "sni".into()), Need::Rebuild);
        assert_eq!(with(|c| c.use_sips_uri = true), Need::Rebuild);
        assert_eq!(with(|c| c.tls_fingerprint = "ab".repeat(32)), Need::Rebuild);
        // Disabling is its own class; enabling is a plain rebuild.
        assert_eq!(with(|c| c.enabled = false), Need::Disable);
        let mut off = base();
        off.enabled = false;
        assert_eq!(classify(&off, &base()), Need::Rebuild);
        // Rebuild wins over re-register when both change.
        assert_eq!(
            with(|c| {
                c.transport = Transport::Tls;
                c.expires = 1;
            }),
            Need::Rebuild
        );
        // An explicit registrar equal to the domain is no change.
        assert_eq!(with(|c| c.registrar = "pbx.example".into()), Need::Hot);
    }

    #[test]
    fn deferral() {
        assert_eq!(outcome(Need::Hot, true, false), UpdateOutcome::Hot);
        assert_eq!(
            outcome(Need::Reregister, true, false),
            UpdateOutcome::Reregistered
        );
        assert_eq!(outcome(Need::Rebuild, false, false), UpdateOutcome::Rebuilt);
        assert_eq!(
            outcome(Need::Rebuild, true, false),
            UpdateOutcome::RebuildDeferred
        );
        assert_eq!(outcome(Need::Rebuild, true, true), UpdateOutcome::Rebuilt);
        // Disabling is never deferred.
        assert_eq!(outcome(Need::Disable, true, false), UpdateOutcome::Rebuilt);
    }
}
