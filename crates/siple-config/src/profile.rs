//! Vendor profiles: bundled defaults for a few account options.
//!
//! Precedence: explicit config value > profile value > built-in default. Explicitness is known
//! only while parsing (a key present in the TOML text), so profiles are resolved there: before
//! deserialising into [`crate::Config`], every `[[account]]` table with `profile = "<vendor>"`
//! gets the vendor's value for each bundled key it does not contain itself. A config that is
//! serialised again therefore carries the resolved values; they then count as explicit.
//!
//! The bundles approximate typical factory behaviour of each family (User-Agent strings are
//! realistic, protocol details are best-effort); every value can be overridden per account.

use toml::Value;

use crate::Profile;

fn s(v: &str) -> Value {
    Value::String(v.into())
}

impl Profile {
    /// The profile named `name` (`generic|yealink|polycom|cisco|grandstream`).
    pub fn from_name(name: &str) -> Option<Self> {
        name.parse_profile()
    }

    /// `key = value` pairs bundled by the profile (account-level option names).
    pub fn bundle(self) -> Vec<(&'static str, Value)> {
        match self {
            Self::Generic => vec![],
            Self::Yealink => vec![
                ("user_agent", s("Yealink SIP-T54W 96.86.0.70")),
                ("hold_style", s("sendonly")),
                ("dtmf_mode", s("rfc4733")),
                ("dnd_reject_code", Value::Integer(480)),
                ("attended_refer_target", s("aor")),
                ("prack", s("disabled")),
                ("session_timers", Value::Boolean(false)),
            ],
            Self::Polycom => vec![
                ("user_agent", s("PolycomVVX-VVX_450-UA/6.4.4.5239")),
                ("hold_style", s("sendonly")),
                ("dtmf_mode", s("rfc4733")),
                ("dnd_reject_code", Value::Integer(486)),
                ("attended_refer_target", s("contact")),
                ("prack", s("supported")),
                ("session_timers", Value::Boolean(true)),
            ],
            Self::Cisco => vec![
                ("user_agent", s("Cisco-CP-8845-3PCC/12.0.1")),
                ("hold_style", s("inactive")),
                ("dtmf_mode", s("rfc4733")),
                ("dnd_reject_code", Value::Integer(480)),
                ("attended_refer_target", s("contact")),
                ("prack", s("supported")),
                ("session_timers", Value::Boolean(true)),
            ],
            Self::Grandstream => vec![
                ("user_agent", s("Grandstream GXP2170 1.0.11.74")),
                ("hold_style", s("sendonly")),
                ("dtmf_mode", s("rfc4733")),
                ("dnd_reject_code", Value::Integer(486)),
                ("attended_refer_target", s("aor")),
                ("prack", s("disabled")),
                ("session_timers", Value::Boolean(true)),
            ],
        }
    }
}

/// Inserts the bundled values of each account's profile for the keys the account table lacks.
pub(crate) fn apply_to_table(root: &mut toml::Table) {
    let Some(Value::Array(accounts)) = root.get_mut("account") else {
        return;
    };
    for acct in accounts {
        let Value::Table(t) = acct else { continue };
        let profile = match t.get("profile").and_then(Value::as_str) {
            Some(name) => match name.parse_profile() {
                Some(p) => p,
                None => continue,
            },
            None => continue,
        };
        for (key, value) in profile.bundle() {
            // An empty string for an "empty = inherit" option counts as not set.
            let unset = matches!(t.get(key), Some(Value::String(v)) if v.is_empty())
                && matches!(key, "user_agent" | "dtmf_mode");
            if unset {
                t.insert(key.into(), value);
            } else {
                t.entry(key).or_insert(value);
            }
        }
    }
}

trait ParseProfile {
    fn parse_profile(&self) -> Option<Profile>;
}

impl ParseProfile for str {
    fn parse_profile(&self) -> Option<Profile> {
        Some(match self {
            "generic" => Profile::Generic,
            "yealink" => Profile::Yealink,
            "polycom" => Profile::Polycom,
            "cisco" => Profile::Cisco,
            "grandstream" => Profile::Grandstream,
            _ => return None,
        })
    }
}
