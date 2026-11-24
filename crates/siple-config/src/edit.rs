//! Typed write-back edits for `config.toml`.
//!
//! [`apply_edits`] is a pure function over a [`toml_edit::DocumentMut`]: it changes exactly the
//! keys an edit names and leaves everything else (comments, blank lines, key order, quoting)
//! alone. Callers re-validate the result with [`crate::Config::parse`] and
//! [`crate::Config::validate`] before writing it anywhere.
//!
//! Rules:
//! - an existing key is updated in place and keeps its decor (the comment after the value);
//! - an absent key whose new value equals the effective default (the vendor profile's value
//!   when the account has one, else the schema default) is not added;
//! - new tables are appended with minimal keys;
//! - unknown sections and keys are rejected, and each value is type-checked per field.
//!
//! Values never echo back in errors (a password must not end up in an error message).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::{AccountConfig, BlfConfig, Config, EXAMPLE_CONFIG, OrInherit, PickupMode, Profile};

/// A typed scalar value of an edit.
///
/// Anything else (an object such as the `{"set": true}` mask of `config.view`, an array, a
/// float, `null`) fails to deserialize, so masked values can never be written back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EditValue {
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A string.
    Str(String),
}

impl EditValue {
    fn from_toml(v: &toml::Value) -> Option<Self> {
        match v {
            toml::Value::Boolean(b) => Some(Self::Bool(*b)),
            toml::Value::Integer(i) => Some(Self::Int(*i)),
            toml::Value::String(s) => Some(Self::Str(s.clone())),
            _ => None,
        }
    }
}

/// The UI-editable keys of an `[[account]]`. Everything else is file-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountField {
    /// `enabled`
    Enabled,
    /// `profile`
    Profile,
    /// `display_name`
    DisplayName,
    /// `username`
    Username,
    /// `auth_username`
    AuthUsername,
    /// `password`
    Password,
    /// `password_store`
    PasswordStore,
    /// `domain`
    Domain,
    /// `registrar`
    Registrar,
    /// `outbound_proxy`
    OutboundProxy,
    /// `transport`
    Transport,
    /// `local_port`
    LocalPort,
    /// `tls_verify`
    TlsVerify,
    /// `tls_fingerprint`
    TlsFingerprint,
    /// `srtp`
    Srtp,
    /// `dnd`
    Dnd,
    /// `auto_answer`
    AutoAnswer,
    /// `answer_delay_ms`
    AnswerDelayMs,
    /// `allow_intercom`
    AllowIntercom,
    /// `auto_hold`
    AutoHold,
    /// `mwi`
    Mwi,
    /// `sip_trace`
    SipTrace,
    /// `pickup`
    Pickup,
    /// `pickup_prefix`
    PickupPrefix,
    /// `pickup_replaces_target`
    PickupReplacesTarget,
    /// `auth_foreign`
    AuthForeign,
}

#[derive(Clone, Copy)]
enum Kind {
    Bool,
    Int(i64, i64),
    Str,
    Enum(&'static [&'static str]),
}

const PROFILES: &[&str] = &["generic", "yealink", "polycom", "cisco", "grandstream"];
const TRANSPORTS: &[&str] = &["udp", "tcp", "tls", "auto"];
const PASSWORD_STORES: &[&str] = &["file", "keyring"];
const SRTP: &[&str] = &["off", "optional", "mandatory"];
const MWI: &[&str] = &["subscribe", "unsolicited", "off"];
const PICKUP: &[&str] = &["off", "prefix", "replaces"];
const REPLACES_TARGET: &[&str] = &["caller", "monitored"];

impl AccountField {
    /// Every field, in the order new accounts are written.
    pub const ALL: [AccountField; 26] = [
        Self::Enabled,
        Self::Profile,
        Self::DisplayName,
        Self::Username,
        Self::AuthUsername,
        Self::Password,
        Self::PasswordStore,
        Self::Domain,
        Self::Registrar,
        Self::OutboundProxy,
        Self::Transport,
        Self::LocalPort,
        Self::TlsVerify,
        Self::TlsFingerprint,
        Self::Srtp,
        Self::Dnd,
        Self::AutoAnswer,
        Self::AnswerDelayMs,
        Self::AllowIntercom,
        Self::AutoHold,
        Self::Mwi,
        Self::SipTrace,
        Self::Pickup,
        Self::PickupPrefix,
        Self::PickupReplacesTarget,
        Self::AuthForeign,
    ];

    /// The `config.toml` key.
    pub fn key(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Profile => "profile",
            Self::DisplayName => "display_name",
            Self::Username => "username",
            Self::AuthUsername => "auth_username",
            Self::Password => "password",
            Self::PasswordStore => "password_store",
            Self::Domain => "domain",
            Self::Registrar => "registrar",
            Self::OutboundProxy => "outbound_proxy",
            Self::Transport => "transport",
            Self::LocalPort => "local_port",
            Self::TlsVerify => "tls_verify",
            Self::TlsFingerprint => "tls_fingerprint",
            Self::Srtp => "srtp",
            Self::Dnd => "dnd",
            Self::AutoAnswer => "auto_answer",
            Self::AnswerDelayMs => "answer_delay_ms",
            Self::AllowIntercom => "allow_intercom",
            Self::AutoHold => "auto_hold",
            Self::Mwi => "mwi",
            Self::SipTrace => "sip_trace",
            Self::Pickup => "pickup",
            Self::PickupPrefix => "pickup_prefix",
            Self::PickupReplacesTarget => "pickup_replaces_target",
            Self::AuthForeign => "auth_foreign",
        }
    }

    fn kind(self) -> Kind {
        match self {
            Self::Enabled
            | Self::TlsVerify
            | Self::Dnd
            | Self::AutoAnswer
            | Self::AllowIntercom
            | Self::AutoHold
            | Self::SipTrace
            | Self::AuthForeign => Kind::Bool,
            Self::Profile => Kind::Enum(PROFILES),
            Self::Transport => Kind::Enum(TRANSPORTS),
            Self::PasswordStore => Kind::Enum(PASSWORD_STORES),
            Self::Srtp => Kind::Enum(SRTP),
            Self::Mwi => Kind::Enum(MWI),
            Self::Pickup => Kind::Enum(PICKUP),
            Self::PickupReplacesTarget => Kind::Enum(REPLACES_TARGET),
            Self::LocalPort => Kind::Int(0, 65_535),
            Self::AnswerDelayMs => Kind::Int(0, 60_000),
            Self::DisplayName
            | Self::Username
            | Self::AuthUsername
            | Self::Password
            | Self::Domain
            | Self::Registrar
            | Self::OutboundProxy
            | Self::TlsFingerprint
            | Self::PickupPrefix => Kind::Str,
        }
    }
}

/// The values of a new account. Keys equal to their default are not written.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountDraft {
    /// The new account's id (`[A-Za-z0-9_.-]`, unique case-insensitively).
    pub id: String,
    /// Fields to set.
    #[serde(default)]
    pub fields: BTreeMap<AccountField, EditValue>,
}

/// One typed edit of `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfigEdit {
    /// Sets one account field.
    SetAccount {
        /// Account id.
        id: String,
        /// Field.
        field: AccountField,
        /// New value.
        value: EditValue,
    },
    /// Removes an account key so the default (or the vendor profile) applies again.
    ResetAccount {
        /// Account id.
        id: String,
        /// Field.
        field: AccountField,
    },
    /// Appends an account; with `replace_example` the shipped, untouched `example` account is
    /// replaced instead.
    AddAccount {
        /// The new account.
        draft: AccountDraft,
        /// Replace the shipped disabled `example` account if it is unchanged.
        #[serde(default)]
        replace_example: bool,
    },
    /// Deletes an account (its table and the comments above it).
    RemoveAccount {
        /// Account id.
        id: String,
    },
    /// Moves an account to position `index` (0-based; past the end means last) in the
    /// `[[account]]` order, which is the display order. Its comments travel with it.
    MoveAccount {
        /// Account id.
        id: String,
        /// Target position.
        index: usize,
    },
    /// Changes an account id.
    RenameAccount {
        /// Current id.
        from: String,
        /// New id.
        to: String,
    },
    /// Replaces an account's BLF list; entries matched by `uri` keep their comments.
    SetBlf {
        /// Account id.
        account: String,
        /// The new list, in order.
        entries: Vec<BlfConfig>,
    },
    /// Sets one key of `calls`, `ui`, `network`, `logging`, `audio` or `general`
    /// (`ui` also takes `shortcuts.<action>`).
    SetSection {
        /// Section name.
        section: String,
        /// Key.
        key: String,
        /// New value.
        value: EditValue,
    },
    /// Removes a section key.
    ResetSection {
        /// Section name.
        section: String,
        /// Key.
        key: String,
    },
}

/// The account id a new account gets from its user and domain: `<username>.<domain>`, every
/// character outside `[A-Za-z0-9_.-]` replaced by `-`.
pub fn suggested_account_id(username: &str, domain: &str) -> String {
    let slug = |s: &str| {
        s.trim()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>()
    };
    format!("{}.{}", slug(username), slug(domain))
}

/// Builds the account a draft describes without touching any file (`account.test` of a draft,
/// before it is saved): the draft goes through the same value checks as `add_account`, then the
/// usual validation. `password_store` is ignored (a draft's `password` is used as given).
/// Problems carry the config path of the offending key and never a submitted value.
pub fn account_from_draft(draft: &AccountDraft) -> Result<AccountConfig, Vec<crate::ConfigIssue>> {
    let issue = |path: String, message: String| vec![crate::ConfigIssue { path, message }];
    let mut draft = draft.clone();
    draft.fields.remove(&AccountField::PasswordStore);
    if draft.id.is_empty() {
        draft.id = "draft".into();
    }
    let mut doc = DocumentMut::new();
    apply_edits(
        &mut doc,
        &[ConfigEdit::AddAccount {
            draft,
            replace_example: false,
        }],
    )
    .map_err(|e| issue(e.path, e.message))?;
    let mut config =
        Config::parse(&doc.to_string()).map_err(|e| issue(String::new(), e.to_string()))?;
    config.validate()?;
    Ok(config.accounts.remove(0))
}

/// Why an edit could not be applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("edit {edit}: {path}: {message}")]
pub struct EditError {
    /// Index of the offending edit in the list.
    pub edit: usize,
    /// Path of the offending key, e.g. `account[0].username`, `ui.theme`.
    pub path: String,
    /// What is wrong (never contains a submitted value).
    pub message: String,
}

struct Fail(String, String);

fn fail<T>(path: impl Into<String>, message: impl Into<String>) -> Result<T, Fail> {
    Err(Fail(path.into(), message.into()))
}

// ---- section schema ----------------------------------------------------------------------------

const HOLD_SCOPE: &[&str] = &["all", "account"];
const THEME: &[&str] = &["system", "light", "dark"];
const DENSITY: &[&str] = &["comfortable", "compact"];
const MOTION: &[&str] = &["system", "on", "off"];
const BLF_VIEW: &[&str] = &["keys", "lamps", "list"];
const BLF_CLICK: &[&str] = &["dial", "prefill"];
const BLF_IN_CALL: &[&str] = &["menu", "consult", "blind"];
const BLF_RINGING: &[&str] = &["pickup", "call"];
const BLF_GROUP: &[&str] = &["account", "group", "none"];
const TRANSFER_ENTER: &[&str] = &["consult", "blind"];
const BACKEND: &[&str] = &["auto", "none", "always"];
const UPDATE_CHANNEL: &[&str] = &["stable", "prerelease"];

/// The keys of each editable section. Other sections (`control`, `media`, `speech`) and the
/// file-only `[general]` keys (`log_dir`, `log_retention_days`, `reconfig_force`: a log directory
/// that an agent could redirect is not a setting to hand out) are not writable.
const SECTIONS: &[(&str, &[(&str, Kind)])] = &[
    (
        "calls",
        &[
            ("hold_scope", Kind::Enum(HOLD_SCOPE)),
            ("decline_code", Kind::Int(400, 699)),
            ("resume_after_failed_transfer", Kind::Bool),
        ],
    ),
    (
        "ui",
        &[
            ("theme", Kind::Enum(THEME)),
            ("density", Kind::Enum(DENSITY)),
            ("reduced_motion", Kind::Enum(MOTION)),
            ("blf_view", Kind::Enum(BLF_VIEW)),
            ("blf_click", Kind::Enum(BLF_CLICK)),
            ("blf_click_in_call", Kind::Enum(BLF_IN_CALL)),
            ("blf_click_ringing", Kind::Enum(BLF_RINGING)),
            ("blf_group_by", Kind::Enum(BLF_GROUP)),
            ("transfer_enter", Kind::Enum(TRANSFER_ENTER)),
            ("tray", Kind::Bool),
            ("close_to_tray", Kind::Bool),
            ("hide_from_taskbar", Kind::Bool),
            ("start_hidden", Kind::Bool),
            ("show_on_incoming", Kind::Bool),
            ("show_on_answer", Kind::Bool),
            ("notifications", Kind::Bool),
            ("handle_links", Kind::Bool),
            ("launch_at_login", Kind::Bool),
            ("developer_tools", Kind::Bool),
        ],
    ),
    (
        "network",
        &[
            ("watch", Kind::Bool),
            ("probe_secs", Kind::Int(0, 3600)),
            ("debounce_ms", Kind::Int(100, 60_000)),
            ("move_calls", Kind::Bool),
        ],
    ),
    (
        "logging",
        &[
            ("redact_srtp_keys", Kind::Bool),
            ("redact_credentials", Kind::Bool),
        ],
    ),
    (
        "audio",
        &[
            ("backend", Kind::Enum(BACKEND)),
            ("input", Kind::Str),
            ("output", Kind::Str),
            ("ringer", Kind::Str),
        ],
    ),
    (
        "general",
        &[("user_agent", Kind::Str), ("log_level", Kind::Str)],
    ),
    (
        "updates",
        &[
            ("check", Kind::Bool),
            ("install", Kind::Bool),
            ("channel", Kind::Enum(UPDATE_CHANNEL)),
        ],
    ),
];

/// Names of the sections [`ConfigEdit::SetSection`] accepts.
pub fn editable_sections() -> Vec<&'static str> {
    SECTIONS.iter().map(|(s, _)| *s).collect()
}

/// Keys of `section` that [`ConfigEdit::SetSection`] accepts (`ui` also takes
/// `shortcuts.<action>`).
pub fn editable_keys(section: &str) -> Vec<&'static str> {
    SECTIONS
        .iter()
        .find(|(s, _)| *s == section)
        .map(|(_, keys)| keys.iter().map(|(k, _)| *k).collect())
        .unwrap_or_default()
}

// ---- value checking --------------------------------------------------------------------------

fn kind_name(k: Kind) -> String {
    match k {
        Kind::Bool => "a boolean".into(),
        Kind::Int(lo, hi) => format!("an integer between {lo} and {hi}"),
        Kind::Str => "a string".into(),
        Kind::Enum(options) => format!("one of {}", options.join(" | ")),
    }
}

fn check(kind: Kind, value: &EditValue) -> Result<Value, String> {
    match (kind, value) {
        (Kind::Bool, EditValue::Bool(b)) => Ok(Value::from(*b)),
        (Kind::Int(lo, hi), EditValue::Int(i)) if (lo..=hi).contains(i) => Ok(Value::from(*i)),
        (Kind::Str, EditValue::Str(s)) => Ok(Value::from(s.as_str())),
        (Kind::Enum(options), EditValue::Str(s)) if options.contains(&s.as_str()) => {
            Ok(Value::from(s.as_str()))
        }
        _ => Err(format!("must be {}", kind_name(kind))),
    }
}

/// Texts that are placeholders for a hidden secret, never a password.
/// Whether `s` is the placeholder of a hidden password rather than a password.
pub fn is_password_mask(s: &str) -> bool {
    MASKS.contains(&s)
}

const MASKS: &[&str] = &["***", "<redacted>", "<set>", "********"];

fn to_value(field: AccountField, value: &EditValue) -> Result<Value, String> {
    if field == AccountField::Password
        && let EditValue::Str(s) = value
        && MASKS.contains(&s.as_str())
    {
        return Err("that is the mask of a hidden password, not a password".into());
    }
    check(field.kind(), value)
}

// ---- defaults --------------------------------------------------------------------------------

fn account_default(table: &Table, key: &str) -> Option<EditValue> {
    if let Some(profile) = table
        .get("profile")
        .and_then(Item::as_str)
        .and_then(Profile::from_name)
        && let Some((_, v)) = profile.bundle().into_iter().find(|(k, _)| *k == key)
    {
        return EditValue::from_toml(&v);
    }
    let defaults = toml::Value::try_from(AccountConfig::default()).ok()?;
    EditValue::from_toml(defaults.get(key)?)
}

fn section_default(section: &str, key: &str) -> Option<EditValue> {
    let defaults = toml::Value::try_from(Config::default()).ok()?;
    EditValue::from_toml(defaults.get(section)?.get(key)?)
}

// ---- table helpers ---------------------------------------------------------------------------

/// Updates `key` in place (keeping its decor), or adds it unless `is_default`.
fn put(
    table: &mut Table,
    key: &str,
    v: Value,
    is_default: bool,
    path: &str,
    stash: &mut Stash,
) -> Result<(), Fail> {
    match table.get_mut(key) {
        Some(item) => match item.as_value_mut() {
            Some(slot) => {
                let decor = slot.decor().clone();
                *slot = v;
                *slot.decor_mut() = decor;
                Ok(())
            }
            None => fail(path, "is not a plain value in the file; edit it by hand"),
        },
        None => {
            if !is_default {
                table.insert(key, Item::Value(v));
                // Reset-then-set in one batch: the key's own comments come back with it.
                if let Some((text, next)) = stash.remove(path)
                    && let Some(mut n) = table.key_mut(&next)
                {
                    let old = prefix_of(n.leaf_decor());
                    if let Some(rest) = old.strip_prefix(&text) {
                        n.leaf_decor_mut().set_prefix(rest.to_string());
                        if let Some(mut k) = table.key_mut(key) {
                            k.leaf_decor_mut().set_prefix(text);
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn accounts_mut(doc: &mut DocumentMut) -> Result<Option<&mut ArrayOfTables>, Fail> {
    match doc.as_table_mut().get_mut("account") {
        None => Ok(None),
        Some(item) => match item.as_array_of_tables_mut() {
            Some(a) => Ok(Some(a)),
            None => fail(
                "account",
                "the file does not use [[account]] tables; edit it by hand",
            ),
        },
    }
}

fn id_of(t: &Table) -> Option<&str> {
    t.get("id").and_then(Item::as_str)
}

fn find_account<'a>(doc: &'a mut DocumentMut, id: &str) -> Result<(usize, &'a mut Table), Fail> {
    let not_found = || Fail(format!("account[id={id}]"), "no such account".into());
    let aot = accounts_mut(doc)?.ok_or_else(not_found)?;
    let idx = aot
        .iter()
        .position(|t| id_of(t) == Some(id))
        .ok_or_else(not_found)?;
    let table = aot.get_mut(idx).ok_or_else(not_found)?;
    Ok((idx, table))
}

fn id_taken(doc: &mut DocumentMut, id: &str, except: Option<usize>) -> Result<bool, Fail> {
    Ok(accounts_mut(doc)?.is_some_and(|aot| {
        aot.iter()
            .enumerate()
            .any(|(i, t)| Some(i) != except && id_of(t).is_some_and(|o| o.eq_ignore_ascii_case(id)))
    }))
}

/// Structural form of an account table (comments, spacing, quoting and key order ignored), for
/// comparison with the shipped example.
fn canonical(table: &Table) -> Option<toml::Value> {
    let mut aot = ArrayOfTables::new();
    aot.push(table.clone());
    let mut doc = DocumentMut::new();
    doc.insert("account", Item::ArrayOfTables(aot));
    let parsed: toml::Table = toml::from_str(&doc.to_string()).ok()?;
    parsed.get("account")?.as_array()?.first().cloned()
}

fn shipped_example() -> Option<toml::Value> {
    let parsed: toml::Table = toml::from_str(EXAMPLE_CONFIG).ok()?;
    parsed.get("account")?.as_array()?.first().cloned()
}

fn is_unchanged_example(table: &Table) -> bool {
    id_of(table) == Some("example")
        && match (canonical(table), shipped_example()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
}

fn section_spec(section: &str, key: &str) -> Result<Kind, Fail> {
    let Some((_, keys)) = SECTIONS.iter().find(|(s, _)| *s == section) else {
        return fail(
            section,
            format!(
                "unknown or file-only section (editable: {})",
                editable_sections().join(", ")
            ),
        );
    };
    if section == "ui"
        && let Some(name) = key.strip_prefix("shortcuts.")
    {
        if name.is_empty() {
            return fail(format!("ui.{key}"), "needs an action name");
        }
        if crate::shortcuts::Shortcut::from_name(name).is_none() {
            return fail(format!("ui.{key}"), "unknown shortcut action");
        }
        return Ok(Kind::Str);
    }
    match keys.iter().find(|(k, _)| *k == key) {
        Some((_, kind)) => Ok(*kind),
        None => fail(
            format!("{section}.{key}"),
            format!(
                "unknown key (editable: {})",
                editable_keys(section).join(", ")
            ),
        ),
    }
}

/// The table a section key lives in, created when `create` is set (`ui.shortcuts.<x>` lives in
/// `[ui.shortcuts]`). Returns the table and the key inside it.
fn section_table<'a>(
    doc: &'a mut DocumentMut,
    section: &str,
    key: &'a str,
    create: bool,
) -> Result<Option<(&'a mut Table, &'a str)>, Fail> {
    let (sub, leaf) = match key.strip_prefix("shortcuts.") {
        Some(name) if section == "ui" => (Some("shortcuts"), name),
        _ => (None, key),
    };
    let root = doc.as_table_mut();
    if root.get(section).is_none() {
        if !create {
            return Ok(None);
        }
        root.insert(section, Item::Table(Table::new()));
    }
    let Some(table) = root.get_mut(section).and_then(Item::as_table_mut) else {
        return fail(section, "is not a table in the file; edit it by hand");
    };
    let Some(sub) = sub else {
        return Ok(Some((table, leaf)));
    };
    if table.get(sub).is_none() {
        if !create {
            return Ok(None);
        }
        let mut t = Table::new();
        t.set_implicit(false);
        table.insert(sub, Item::Table(t));
    }
    match table.get_mut(sub).and_then(Item::as_table_mut) {
        Some(t) => Ok(Some((t, leaf))),
        None => fail(
            format!("{section}.{sub}"),
            "is not a table in the file; edit it by hand",
        ),
    }
}

// ---- removal keeps the neighbors' comments ----------------------------------------------------

/// What `toml_edit` stores as an item's prefix is everything since the previous item: the
/// previous item's trailing comments and blank lines, then the comment block directly above this
/// one. Splits at the last blank line into (what may belong to the neighbors, the removed item's own
/// directly adjacent comment block).
fn split_prefix(prefix: &str) -> (String, String) {
    // Only the block directly adjacent to the header (no blank line between) is the item's
    // own. Anything above a blank line may belong to a neighbor and is never deleted.
    let mut end = 0;
    let mut pos = 0;
    for line in prefix.split_inclusive('\n') {
        pos += line.len();
        if line.trim().is_empty() {
            end = pos;
        }
    }
    (prefix[..end].to_string(), prefix[end..].to_string())
}

fn prefix_of(decor: &toml_edit::Decor) -> String {
    decor
        .prefix()
        .and_then(toml_edit::RawString::as_str)
        .unwrap_or("")
        .to_string()
}

/// Puts `keep` in front of the prefix of `next`. Returns false without one.
fn hand_over(keep: &str, next: Option<&mut Table>) -> bool {
    match next {
        Some(t) => {
            let old = prefix_of(t.decor());
            t.decor_mut().set_prefix(format!("{keep}{old}"));
            true
        }
        None => false,
    }
}

fn suffix_of(decor: &toml_edit::Decor) -> String {
    decor
        .suffix()
        .and_then(toml_edit::RawString::as_str)
        .unwrap_or("")
        .to_string()
}

/// Text that follows the line of `decor` (value or table header): `text` goes on the lines
/// below it.
fn append_below(decor: &mut toml_edit::Decor, text: &str) {
    let old = suffix_of(decor);
    let text = text.strip_suffix('\n').unwrap_or(text);
    decor.set_suffix(format!("{old}\n{text}"));
}

/// Appends `text` below the last value of `item` in file order (sub-tables print after their
/// parent's own keys). Returns false when there is no value at all.
fn append_to_last_item(item: &mut Item, text: &str) -> bool {
    match item {
        Item::Value(v) => {
            append_below(v.decor_mut(), text);
            true
        }
        Item::Table(t) => append_to_last(t, text),
        Item::ArrayOfTables(a) => (0..a.len())
            .rev()
            .any(|i| a.get_mut(i).is_some_and(|t| append_to_last(t, text))),
        Item::None => false,
    }
}

fn append_to_last(table: &mut Table, text: &str) -> bool {
    // Sub-tables come last in the file; then the last plain value.
    let mut values = Vec::new();
    let mut tables = Vec::new();
    for (k, item) in table.iter() {
        if item.is_value() {
            values.push(k.to_string());
        } else {
            tables.push(k.to_string());
        }
    }
    for k in tables.iter().rev().chain(values.iter().rev()) {
        if table
            .get_mut(k)
            .is_some_and(|i| append_to_last_item(i, text))
        {
            return true;
        }
    }
    false
}

/// Appends `text` below the last value of the root items that come before `key` in the file.
fn append_before_root_key(doc: &mut DocumentMut, key: &str, text: &str) -> bool {
    let root = doc.as_table_mut();
    let keys: Vec<String> = root.iter().map(|(k, _)| k.to_string()).collect();
    let Some(at) = keys.iter().position(|k| k == key) else {
        return false;
    };
    keys[..at].iter().rev().any(|k| {
        root.get_mut(k)
            .is_some_and(|i| append_to_last_item(i, text))
    })
}

fn keep_in_trailing(doc: &mut DocumentMut, keep: &str) {
    let old = doc.trailing().as_str().unwrap_or("").to_string();
    doc.set_trailing(format!("{keep}{old}"));
}

/// Comments moved by `remove_key`, by key path: (the text, the key it was handed to), so a later
/// `put` of the same key in the same batch takes them back.
type Stash = std::collections::HashMap<String, (String, String)>;

/// Removes `key` from `table`; every comment above it moves to the next key, else below the
/// previous value, else below the table's header, so only the key itself disappears.
fn remove_key(table: &mut Table, key: &str, path: &str, stash: &mut Stash) {
    let keys: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
    let Some(i) = keys.iter().position(|k| k == key) else {
        return;
    };
    let prefix = table
        .key(key)
        .map(|k| prefix_of(k.leaf_decor()))
        .unwrap_or_default();
    table.remove(key);
    if prefix.trim().is_empty() {
        return;
    }
    let is_value = |t: &Table, k: &str| t.get(k).is_some_and(Item::is_value);
    if let Some(next) = keys.get(i + 1).filter(|k| is_value(table, k)) {
        if let Some(mut k) = table.key_mut(next) {
            let old = prefix_of(k.leaf_decor());
            k.leaf_decor_mut().set_prefix(format!("{prefix}{old}"));
            stash.insert(path.to_string(), (prefix, next.clone()));
        }
    } else if let Some(prev) = i
        .checked_sub(1)
        .map(|j| keys[j].as_str())
        .filter(|k| is_value(table, k))
        && let Some(v) = table.get_mut(prev).and_then(Item::as_value_mut)
    {
        append_below(v.decor_mut(), &prefix);
    } else {
        append_below(table.decor_mut(), &prefix);
    }
}

// ---- applying --------------------------------------------------------------------------------

/// Applies `edits` in order. Either all of them apply or `doc` is left untouched.
pub fn apply_edits(doc: &mut DocumentMut, edits: &[ConfigEdit]) -> Result<(), EditError> {
    let mut work = doc.clone();
    let mut stash = Stash::new();
    for (edit, e) in edits.iter().enumerate() {
        apply_one(&mut work, e, &mut stash).map_err(|Fail(path, message)| EditError {
            edit,
            path,
            message,
        })?;
    }
    *doc = work;
    Ok(())
}

fn apply_one(doc: &mut DocumentMut, edit: &ConfigEdit, stash: &mut Stash) -> Result<(), Fail> {
    match edit {
        ConfigEdit::SetAccount { id, field, value } => {
            let (idx, table) = find_account(doc, id)?;
            let path = format!("account[{idx}].{}", field.key());
            let v = to_value(*field, value).map_err(|m| Fail(path.clone(), m))?;
            let is_default = account_default(table, field.key()).as_ref() == Some(value);
            put(table, field.key(), v, is_default, &path, stash)
        }
        ConfigEdit::ResetAccount { id, field } => {
            let (idx, table) = find_account(doc, id)?;
            let path = format!("account[{idx}].{}", field.key());
            remove_key(table, field.key(), &path, stash);
            Ok(())
        }
        ConfigEdit::AddAccount {
            draft,
            replace_example,
        } => add_account(doc, draft, *replace_example),
        ConfigEdit::RemoveAccount { id } => {
            let (idx, _) = find_account(doc, id)?;
            let aot = accounts_mut(doc)?.expect("account found above");
            let removed = aot.remove(idx);
            // Only the comment block directly above the account goes with it; what belongs to
            // the item before it stays in file order: below the previous account's last value,
            // else below the last value before the accounts, else above the next account.
            let (keep, _) = split_prefix(&prefix_of(removed.decor()));
            let mut placed = keep.is_empty();
            if !placed && idx > 0 {
                placed = aot
                    .get_mut(idx - 1)
                    .is_some_and(|prev| append_to_last(prev, &keep));
            }
            if !placed && idx == 0 {
                placed = hand_over(&keep, aot.get_mut(idx));
            }
            if aot.is_empty() {
                doc.as_table_mut().remove("account");
            }
            if !placed && idx == 0 {
                placed = append_before_root_key(doc, "account", &keep);
            }
            if !placed {
                keep_in_trailing(doc, &keep);
            }
            Ok(())
        }
        ConfigEdit::MoveAccount { id, index } => {
            let (from, _) = find_account(doc, id)?;
            let aot = accounts_mut(doc)?.expect("account found above");
            let to = (*index).min(aot.len() - 1);
            if from == to {
                return Ok(());
            }
            // Slots keep their blank-line separation and neighbors' comments; only the comment
            // block directly above each account moves with it. Tables print by `position`: the
            // positions of every account-owned table (accounts and their sub-tables) are
            // collected, sorted and handed out again in the new depth-first order, so the
            // slots that other tables (`[ui]`, `[calls]`, ...) occupy between accounts are
            // untouched.
            let n = aot.len();
            let mut tables: Vec<Table> = (0..n).map(|_| aot.remove(0)).collect();
            let mut slots: Vec<isize> = Vec::new();
            tables.iter().for_each(|t| collect_positions(t, &mut slots));
            slots.sort_unstable();
            let first_slot = slots.first().copied();
            let mut slots = slots.into_iter();
            let keeps: Vec<String> = tables
                .iter()
                .map(|t| split_prefix(&prefix_of(t.decor())).0)
                .collect();
            let moved = tables.remove(from);
            tables.insert(to, moved);
            for (t, keep) in tables.iter_mut().zip(keeps) {
                let own = split_prefix(&prefix_of(t.decor())).1;
                t.decor_mut().set_prefix(format!("{keep}{own}"));
                assign_positions(t, &mut slots);
            }
            // An account added earlier in the same batch has no position and would print after
            // the table visited before it (which may follow the other accounts). Leading
            // unpositioned accounts take the first slot; array order breaks the tie.
            if let Some(first) = first_slot {
                for t in tables.iter_mut().take_while(|t| t.position().is_none()) {
                    t.set_position(Some(first));
                }
            }
            for t in tables {
                aot.push(t);
            }
            Ok(())
        }
        ConfigEdit::RenameAccount { from, to } => {
            if !valid_id(to) {
                return fail(
                    "account.id",
                    "may only contain A-Z a-z 0-9 _ . - and not be empty",
                );
            }
            let (idx, _) = find_account(doc, from)?;
            if id_taken(doc, to, Some(idx))? {
                return fail(
                    format!("account[{idx}].id"),
                    "another account already has this id",
                );
            }
            let (_, table) = find_account(doc, from)?;
            put(
                table,
                "id",
                Value::from(to.as_str()),
                false,
                "account.id",
                stash,
            )
        }
        ConfigEdit::SetBlf { account, entries } => set_blf(doc, account, entries, stash),
        ConfigEdit::SetSection {
            section,
            key,
            value,
        } => {
            let kind = section_spec(section, key)?;
            let path = format!("{section}.{key}");
            let v = check(kind, value).map_err(|m| Fail(path.clone(), m))?;
            let is_default = section_default(section, key).as_ref() == Some(value);
            // Nothing to write for an absent key at its default: do not create the section.
            let exists = section_table(doc, section, key, false)?
                .is_some_and(|(t, leaf)| t.contains_key(leaf));
            if !exists && is_default {
                return Ok(());
            }
            let (table, leaf) = section_table(doc, section, key, true)?.expect("created");
            put(table, leaf, v, is_default, &path, stash)
        }
        ConfigEdit::ResetSection { section, key } => {
            // Any `shortcuts.<name>` can be removed, so a bad entry can always be repaired.
            let shortcut_entry = section == "ui"
                && key
                    .strip_prefix("shortcuts.")
                    .is_some_and(|n| !n.is_empty());
            if !shortcut_entry {
                section_spec(section, key)?;
            }
            if let Some((table, leaf)) = section_table(doc, section, key, false)? {
                remove_key(table, leaf, &format!("{section}.{key}"), stash);
            }
            Ok(())
        }
    }
}

/// Positions of `t` and the tables nested in it, depth-first.
fn collect_positions(t: &Table, out: &mut Vec<isize>) {
    out.extend(t.position());
    for (_, item) in t.iter() {
        match item {
            Item::Table(sub) => collect_positions(sub, out),
            Item::ArrayOfTables(a) => a.iter().for_each(|x| collect_positions(x, out)),
            _ => {}
        }
    }
}

/// Hands `slots` to `t` and its nested tables, depth-first; tables without a position (they
/// follow their parent) keep none.
fn assign_positions(t: &mut Table, slots: &mut impl Iterator<Item = isize>) {
    if t.position().is_some() {
        t.set_position(slots.next());
    }
    for (_, item) in t.iter_mut() {
        match item {
            Item::Table(sub) => assign_positions(sub, slots),
            Item::ArrayOfTables(a) => a.iter_mut().for_each(|x| assign_positions(x, slots)),
            _ => {}
        }
    }
}

fn add_account(
    doc: &mut DocumentMut,
    draft: &AccountDraft,
    replace_example: bool,
) -> Result<(), Fail> {
    if !valid_id(&draft.id) {
        return fail(
            "account.id",
            "may only contain A-Z a-z 0-9 _ . - and not be empty",
        );
    }
    let mut table = Table::new();
    table.insert("id", Item::Value(Value::from(draft.id.as_str())));
    for (field, value) in &draft.fields {
        let path = format!("account[{}].{}", draft.id, field.key());
        let v = to_value(*field, value).map_err(|m| Fail(path.clone(), m))?;
        let default = toml::Value::try_from(AccountConfig::default())
            .ok()
            .and_then(|d| d.get(field.key()).and_then(EditValue::from_toml));
        if default.as_ref() != Some(value) {
            table.insert(field.key(), Item::Value(v));
        }
    }
    let example_idx = if replace_example {
        accounts_mut(doc)?.and_then(|aot| aot.iter().position(is_unchanged_example))
    } else {
        None
    };
    if id_taken(doc, &draft.id, example_idx)? {
        return fail("account.id", "an account with this id already exists");
    }
    if let Some(idx) = example_idx {
        let aot = accounts_mut(doc)?.expect("found above");
        if let Some(slot) = aot.get_mut(idx) {
            // The example's own comment block goes; the previous section's trailing note stays.
            let (keep, _) = split_prefix(&prefix_of(slot.decor()));
            table.decor_mut().set_prefix(keep);
            *slot = table;
            return Ok(());
        }
    }
    if doc.as_table().get("account").is_none() {
        doc.as_table_mut()
            .insert("account", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    // A blank line between accounts.
    table.decor_mut().set_prefix("\n");
    accounts_mut(doc)?.expect("created above").push(table);
    Ok(())
}

fn set_blf(
    doc: &mut DocumentMut,
    account: &str,
    entries: &[BlfConfig],
    stash: &mut Stash,
) -> Result<(), Fail> {
    let (idx, table) = find_account(doc, account)?;
    let path = format!("account[{idx}].blf");
    let old: Vec<Table> = match table.get("blf") {
        None => Vec::new(),
        Some(item) => match item.as_array_of_tables() {
            Some(a) => a.iter().cloned().collect(),
            None => {
                return fail(
                    path,
                    "is not a list of [[account.blf]] tables; edit it by hand",
                );
            }
        },
    };
    let mut remaining = old;
    let mut carry = String::new();
    let mut new = ArrayOfTables::new();
    for (j, entry) in entries.iter().enumerate() {
        let at = remaining
            .iter()
            .position(|t| t.get("uri").and_then(Item::as_str) == Some(entry.uri.as_str()));
        let mut t = match at {
            Some(i) => remaining.remove(i),
            None => {
                let mut t = Table::new();
                t.insert("uri", Item::Value(Value::from(entry.uri.as_str())));
                t
            }
        };
        let pickup = match entry.pickup {
            OrInherit::Inherit => String::new(),
            OrInherit::Value(PickupMode::Off) => "off".into(),
            OrInherit::Value(PickupMode::Prefix) => "prefix".into(),
            OrInherit::Value(PickupMode::Replaces) => "replaces".into(),
        };
        // `label` stays in place when cleared (as before); the optional keys go when empty.
        let fields = [
            ("label", entry.label.as_str(), false),
            ("group", entry.group.as_str(), true),
            ("pickup", pickup.as_str(), true),
            ("pickup_prefix", entry.pickup_prefix.as_str(), true),
            ("pickup_target", entry.pickup_target.as_str(), true),
        ];
        for (key, value, drop_when_empty) in fields {
            let key_path = format!("{path}[{j}].{key}");
            if value.is_empty() && drop_when_empty {
                remove_key(&mut t, key, &key_path, stash);
            } else {
                put(
                    &mut t,
                    key,
                    Value::from(value),
                    value.is_empty(),
                    &key_path,
                    stash,
                )?;
            }
        }
        // Tables print by their `position`; unset, they follow the account in list order.
        t.set_position(None);
        new.push(t);
    }
    // Dropped entries lose their own comment block only.
    for dropped in &remaining {
        carry.push_str(&split_prefix(&prefix_of(dropped.decor())).0);
    }
    if new.is_empty() {
        table.remove("blf");
        if !carry.is_empty() && !append_to_last(table, &carry) {
            keep_in_trailing(doc, &carry);
        }
    } else {
        if let Some(first) = new.get_mut(0) {
            let old = prefix_of(first.decor());
            first.decor_mut().set_prefix(format!("{carry}{old}"));
        }
        table.insert("blf", Item::ArrayOfTables(new));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"# Top comment

[general]
log_level = "info"   # keep this

# Calls section
[calls]
hold_scope = "all"  # hold comment

# --- Desk phone ---
[[account]]
id = "desk"  # id comment
username = "1001"   # ext
password = "pw"
domain = "pbx.example.com"

# BLF: boss
[[account.blf]]
uri = "1002"   # boss ext
label = "Boss"

[[account.blf]]
uri = "1003"
label = "Carl"

# --- Second ---
[[account]]
id = "second"
username = "2001"
domain = "pbx.example.com"
"#;

    fn doc(text: &str) -> DocumentMut {
        text.parse().unwrap()
    }

    fn run(text: &str, edits: &[ConfigEdit]) -> String {
        let mut d = doc(text);
        apply_edits(&mut d, edits).unwrap_or_else(|e| panic!("{e}"));
        let out = d.to_string();
        let cfg = Config::parse(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
        assert_eq!(cfg.validate(), Ok(()), "{out}");
        out
    }

    fn err(text: &str, edits: &[ConfigEdit]) -> EditError {
        let mut d = doc(text);
        let before = d.to_string();
        let e = apply_edits(&mut d, edits).unwrap_err();
        assert_eq!(
            d.to_string(),
            before,
            "a failed edit leaves the document alone"
        );
        e
    }

    fn set(id: &str, field: AccountField, v: impl Into<EditValue>) -> ConfigEdit {
        ConfigEdit::SetAccount {
            id: id.into(),
            field,
            value: v.into(),
        }
    }

    fn sec(section: &str, key: &str, v: impl Into<EditValue>) -> ConfigEdit {
        ConfigEdit::SetSection {
            section: section.into(),
            key: key.into(),
            value: v.into(),
        }
    }

    impl From<bool> for EditValue {
        fn from(b: bool) -> Self {
            Self::Bool(b)
        }
    }
    impl From<i64> for EditValue {
        fn from(i: i64) -> Self {
            Self::Int(i)
        }
    }
    impl From<&str> for EditValue {
        fn from(s: &str) -> Self {
            Self::Str(s.into())
        }
    }

    #[test]
    fn set_account_updates_in_place_and_keeps_every_comment() {
        let out = run(DOC, &[set("desk", AccountField::Username, "1005")]);
        assert_eq!(
            out,
            DOC.replace("username = \"1001\"   # ext", "username = \"1005\"   # ext")
        );
        let out = run(DOC, &[set("desk", AccountField::Dnd, true)]);
        // A new key is appended to the account's own keys; everything else is untouched.
        assert!(
            out.contains("domain = \"pbx.example.com\"\ndnd = true\n"),
            "{out}"
        );
        assert!(out.contains("# --- Desk phone ---") && out.contains("# BLF: boss"));
        assert!(out.contains("uri = \"1002\"   # boss ext"));
    }

    #[test]
    fn auth_foreign_is_an_editable_bool() {
        let out = run(DOC, &[set("desk", AccountField::AuthForeign, true)]);
        assert!(out.contains("\nauth_foreign = true\n"), "{out}");
        let c = crate::Config::parse(&out).unwrap();
        assert!(
            c.accounts
                .iter()
                .find(|a| a.id == "desk")
                .unwrap()
                .auth_foreign
        );
        let e = err(DOC, &[set("desk", AccountField::AuthForeign, "yes")]);
        assert_eq!(e.path, "account[0].auth_foreign");
    }

    #[test]
    fn absent_key_at_default_is_not_added_but_present_key_is_updated() {
        // enabled defaults to true, dnd to false, transport to udp.
        let same = run(
            DOC,
            &[
                set("desk", AccountField::Enabled, true),
                set("desk", AccountField::Dnd, false),
                set("desk", AccountField::Transport, "udp"),
                set("desk", AccountField::LocalPort, 0i64),
                set("desk", AccountField::Password, "pw"),
                sec("ui", "theme", "system"),
                sec("calls", "decline_code", 486i64),
                sec("audio", "input", ""),
            ],
        );
        // Only the (present) password key is rewritten, to the same text.
        assert_eq!(same, DOC);
        // A present key at the default value stays and is updated to it.
        let d = DOC.replace(
            "hold_scope = \"all\"  # hold comment",
            "hold_scope = \"account\"  # hold comment",
        );
        let out = run(&d, &[sec("calls", "hold_scope", "all")]);
        assert_eq!(out, DOC);
    }

    #[test]
    fn default_follows_the_vendor_profile() {
        let t = "[[account]]\nid = \"y\"\nprofile = \"yealink\"\n";
        let d = doc(t);
        let table = d["account"].as_array_of_tables().unwrap().get(0).unwrap();
        // Yealink fills dnd_reject_code = 480: the schema default 486 is NOT the effective default.
        assert_eq!(
            account_default(table, "dnd_reject_code"),
            Some(EditValue::Int(480))
        );
        assert_eq!(account_default(table, "dnd"), Some(EditValue::Bool(false)));
        let g = doc("[[account]]\nid = \"g\"\n");
        let table = g["account"].as_array_of_tables().unwrap().get(0).unwrap();
        assert_eq!(
            account_default(table, "dnd_reject_code"),
            Some(EditValue::Int(486))
        );
    }

    #[test]
    fn type_and_range_errors_carry_a_path_and_never_echo_values() {
        let e = err(DOC, &[set("desk", AccountField::Dnd, "yes")]);
        assert_eq!((e.edit, e.path.as_str()), (0, "account[0].dnd"));
        assert!(e.message.contains("boolean"), "{e}");
        let e = err(DOC, &[set("second", AccountField::LocalPort, 70_000i64)]);
        assert_eq!(e.path, "account[1].local_port");
        let e = err(
            DOC,
            &[set("desk", AccountField::Transport, "carrier-pigeon")],
        );
        assert!(
            e.message.contains("udp | tcp | tls") && !e.message.contains("pigeon"),
            "{e}"
        );
        let e = err(DOC, &[sec("ui", "theme", "blue")]);
        assert_eq!(e.path, "ui.theme");
        assert!(!e.message.contains("blue"));
        let e = err(DOC, &[set("desk", AccountField::Password, 5i64)]);
        assert_eq!(e.path, "account[0].password");
        // The second edit fails: the first is not applied either.
        let e = err(
            DOC,
            &[
                set("desk", AccountField::Dnd, true),
                set("nobody", AccountField::Dnd, true),
            ],
        );
        assert_eq!((e.edit, e.path.as_str()), (1, "account[id=nobody]"));
    }

    #[test]
    fn masked_password_is_never_accepted() {
        for mask in ["***", "<redacted>", "<set>"] {
            let e = err(DOC, &[set("desk", AccountField::Password, mask)]);
            assert!(e.message.contains("mask"), "{e}");
        }
        // The `{"set": true}` object of config.view does not even deserialize as a value.
        let bad = r#"{"op":"set_account","id":"desk","field":"password","value":{"set":true}}"#;
        assert!(serde_json::from_str::<ConfigEdit>(bad).is_err());
        let ok = r#"{"op":"set_account","id":"desk","field":"password","value":"s3cret"}"#;
        assert!(serde_json::from_str::<ConfigEdit>(ok).is_ok());
    }

    #[test]
    fn reset_account_removes_the_key() {
        let out = run(
            DOC,
            &[ConfigEdit::ResetAccount {
                id: "desk".into(),
                field: AccountField::Password,
            }],
        );
        assert_eq!(out, DOC.replace("password = \"pw\"\n", ""));
        // Resetting an absent key is a no-op.
        let same = run(
            DOC,
            &[ConfigEdit::ResetAccount {
                id: "desk".into(),
                field: AccountField::Dnd,
            }],
        );
        assert_eq!(same, DOC);
    }

    fn draft(id: &str, fields: &[(AccountField, EditValue)]) -> AccountDraft {
        AccountDraft {
            id: id.into(),
            fields: fields.iter().cloned().collect(),
        }
    }

    #[test]
    fn add_account_appends_minimal_keys() {
        let out = run(
            DOC,
            &[ConfigEdit::AddAccount {
                draft: draft(
                    "new",
                    &[
                        (AccountField::Username, "3001".into()),
                        (AccountField::Domain, "pbx.example.com".into()),
                        (AccountField::Enabled, true.into()),
                        (AccountField::Dnd, false.into()),
                        (AccountField::Transport, "tls".into()),
                    ],
                ),
                replace_example: false,
            }],
        );
        let tail = out.strip_prefix(DOC).expect("existing text is untouched");
        assert_eq!(
            tail,
            "\n[[account]]\nid = \"new\"\nusername = \"3001\"\ndomain = \"pbx.example.com\"\ntransport = \"tls\"\n"
        );
        // Into a file without accounts.
        let out = run(
            "[general]\nlog_level = \"info\"\n",
            &[ConfigEdit::AddAccount {
                draft: draft(
                    "a",
                    &[
                        (AccountField::Username, "1".into()),
                        (AccountField::Domain, "d".into()),
                    ],
                ),
                replace_example: true,
            }],
        );
        assert!(out.contains("[[account]]\nid = \"a\""), "{out}");
        let e = err(
            DOC,
            &[ConfigEdit::AddAccount {
                draft: draft("DESK", &[]),
                replace_example: false,
            }],
        );
        assert!(e.message.contains("already exists"));
        let e = err(
            DOC,
            &[ConfigEdit::AddAccount {
                draft: draft("bad id", &[]),
                replace_example: false,
            }],
        );
        assert_eq!(e.path, "account.id");
    }

    #[test]
    fn add_account_replaces_only_an_untouched_example() {
        let add = |replace| ConfigEdit::AddAccount {
            draft: draft(
                "mine",
                &[
                    (AccountField::Username, "1001".into()),
                    (AccountField::Domain, "pbx.test".into()),
                ],
            ),
            replace_example: replace,
        };
        // The shipped file, with some extra user comments and different spacing.
        let shipped = crate::EXAMPLE_CONFIG.replace(
            "display_name = \"Example\"",
            "display_name   =   'Example'  # mine",
        );
        let out = run(&shipped, &[add(true)]);
        let cfg = Config::parse(&out).unwrap();
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].id, "mine");
        assert!(
            !out.contains("Sample account"),
            "its leading comment goes with it"
        );
        // The sections above are untouched.
        assert!(out.contains("[speech]") && out.contains("tts_command"));

        // Appended, not replaced, when not asked to.
        let out = run(crate::EXAMPLE_CONFIG, &[add(false)]);
        assert_eq!(Config::parse(&out).unwrap().accounts.len(), 2);

        // Not replaced once the user changed it.
        let edited = crate::EXAMPLE_CONFIG.replace("username = \"1001\"", "username = \"1999\"");
        let out = run(&edited, &[add(true)]);
        let cfg = Config::parse(&out).unwrap();
        assert_eq!(cfg.accounts.len(), 2);
        assert_eq!(cfg.accounts[0].username, "1999");
        // ... or enabled.
        let enabled = crate::EXAMPLE_CONFIG.replace("enabled = false", "enabled = true");
        assert_eq!(
            Config::parse(&run(&enabled, &[add(true)]))
                .unwrap()
                .accounts
                .len(),
            2
        );
    }

    #[test]
    fn remove_account_drops_its_table_and_leading_comment() {
        let out = run(DOC, &[ConfigEdit::RemoveAccount { id: "desk".into() }]);
        assert!(!out.contains("Desk phone") && !out.contains("BLF: boss") && !out.contains("1001"));
        assert!(out.contains("# --- Second ---") && out.contains("# Calls section"));
        assert_eq!(Config::parse(&out).unwrap().accounts.len(), 1);
        let out = run(
            &out,
            &[ConfigEdit::RemoveAccount {
                id: "second".into(),
            }],
        );
        assert!(!out.contains("account"), "{out}");
        assert!(out.contains("# Top comment"));
        let e = err(DOC, &[ConfigEdit::RemoveAccount { id: "x".into() }]);
        assert_eq!(e.path, "account[id=x]");
    }

    fn mv(id: &str, index: usize) -> ConfigEdit {
        ConfigEdit::MoveAccount {
            id: id.into(),
            index,
        }
    }

    fn ids(text: &str) -> Vec<String> {
        Config::parse(text)
            .unwrap()
            .accounts
            .iter()
            .map(|a| a.id.clone())
            .collect()
    }

    /// Three accounts, a comment block each, a BLF sub-table on `a`, and a section after them.
    fn move_doc() -> String {
        "# file header\n\n# --- a ---\n[[account]]\nid = \"a\"\nusername = \"1\"\ndomain = \"d\"\n\n\
         # blf of a\n[[account.blf]]\nuri = \"9\"\nlabel = \"Nine\"\n\n\
         # --- b ---\n[[account]]\nid = \"b\"\nusername = \"2\"\ndomain = \"d\"\n# trailing b\n\n\
         # --- c ---\n[[account]]\nid = \"c\"\nusername = \"3\"\ndomain = \"d\"\n\n\
         # ui section\n[ui]\ntray = false\n"
            .into()
    }

    #[test]
    fn move_account_first_to_last_and_back() {
        let d = move_doc();
        let out = run(&d, &[mv("a", 2)]);
        assert_eq!(ids(&out), ["b", "c", "a"], "{out}");
        assert_eq!(Config::parse(&out).unwrap().accounts[2].blf.len(), 1);
        assert!(out.starts_with("# file header\n\n# --- b ---"), "{out}");
        assert!(
            out.trim_end().ends_with("# ui section\n[ui]\ntray = false"),
            "{out}"
        );
        // Comment of each account stays directly above it.
        let pos = |t: &str, n: &str| t.find(n).unwrap();
        assert!(pos(&out, "# --- a ---") < pos(&out, "id = \"a\""));
        assert!(pos(&out, "id = \"c\"") < pos(&out, "# --- a ---"));
        assert!(pos(&out, "# blf of a") > pos(&out, "id = \"a\""));
        let back = run(&out, &[mv("a", 0)]);
        assert_eq!(back, d);
    }

    #[test]
    fn move_account_last_to_first_and_middle() {
        let d = move_doc();
        let out = run(&d, &[mv("c", 0)]);
        assert_eq!(ids(&out), ["c", "a", "b"], "{out}");
        // The comment block above each account travels with it; the gap before the slot stays.
        assert!(
            out.contains("# --- c ---\n[[account]]\nid = \"c\""),
            "{out}"
        );
        assert!(out.contains("# trailing b\n\n# --- b ---"), "{out}");
        assert_eq!(run(&out, &[mv("c", 2)]), d);
        let out = run(&d, &[mv("a", 1)]);
        assert_eq!(ids(&out), ["b", "a", "c"], "{out}");
        let out = run(&d, &[mv("c", 1)]);
        assert_eq!(ids(&out), ["a", "c", "b"], "{out}");
    }

    #[test]
    fn move_account_keeps_interleaved_tables_in_their_slots() {
        let d = "[[account]]\nid = \"a\"\nusername = \"1\"\ndomain = \"d\"\n\n[ui]\ntray = false\n\n\
                 [[account]]\nid = \"b\"\nusername = \"2\"\ndomain = \"d\"\n\n[calls]\nhold_scope = \"all\"\n\n\
                 [[account]]\nid = \"c\"\nusername = \"3\"\ndomain = \"d\"\n";
        let out = run(d, &[mv("a", 2)]);
        assert_eq!(
            out,
            "[[account]]\nid = \"b\"\nusername = \"2\"\ndomain = \"d\"\n\n[ui]\ntray = false\n\n\
             [[account]]\nid = \"c\"\nusername = \"3\"\ndomain = \"d\"\n\n[calls]\nhold_scope = \"all\"\n\n\
             [[account]]\nid = \"a\"\nusername = \"1\"\ndomain = \"d\"\n"
        );
    }

    #[test]
    fn move_account_keeps_a_trailing_ui_shortcuts_table_last() {
        let d = "[[account]]\nid = \"a\"\nusername = \"1\"\ndomain = \"d\"\n\n\
                 [[account]]\nid = \"b\"\nusername = \"2\"\ndomain = \"d\"\n\n\
                 [ui.shortcuts]\nanswer = \"Ctrl+A\"\n";
        let out = run(d, &[mv("b", 0)]);
        assert_eq!(
            out,
            "[[account]]\nid = \"b\"\nusername = \"2\"\ndomain = \"d\"\n\n\
             [[account]]\nid = \"a\"\nusername = \"1\"\ndomain = \"d\"\n\n\
             [ui.shortcuts]\nanswer = \"Ctrl+A\"\n"
        );
    }

    /// An account added earlier in the same batch has no table position; moving it first must
    /// still print it first (toml_edit would otherwise place it after `[ui.shortcuts]`).
    #[test]
    fn move_account_added_in_same_batch_to_first() {
        let d = "[ui]\ntray = false\n\n\
                 [[account]]\nid = \"a\"\nusername = \"1\"\ndomain = \"d\"\n\n\
                 [[account]]\nid = \"b\"\nusername = \"2\"\ndomain = \"d\"\n\n\
                 [ui.shortcuts]\nanswer = \"F1\"\n";
        let out = run(
            d,
            &[
                ConfigEdit::AddAccount {
                    draft: draft(
                        "n",
                        &[
                            (AccountField::Username, "3".into()),
                            (AccountField::Domain, "d".into()),
                        ],
                    ),
                    replace_example: false,
                },
                mv("n", 0),
            ],
        );
        assert_eq!(ids(&out), ["n", "a", "b"], "{out}");
        assert!(out.starts_with("[ui]\ntray = false\n"), "{out}");
        assert!(
            out.trim_end().ends_with("[ui.shortcuts]\nanswer = \"F1\""),
            "{out}"
        );
    }

    #[test]
    fn move_account_edge_cases() {
        let d = move_doc();
        assert_eq!(ids(&run(&d, &[mv("a", 99)])), ["b", "c", "a"]);
        for (id, i) in [("a", 0), ("b", 1), ("c", 2), ("c", 50)] {
            assert_eq!(run(&d, &[mv(id, i)]), d, "{id} {i}");
        }
        let e = err(&d, &[mv("nope", 0)]);
        assert_eq!(e.path, "account[id=nope]");
    }

    #[test]
    fn rename_account_keeps_decor_and_checks_uniqueness() {
        let out = run(
            DOC,
            &[ConfigEdit::RenameAccount {
                from: "desk".into(),
                to: "desk-2".into(),
            }],
        );
        assert_eq!(
            out,
            DOC.replace(
                "id = \"desk\"  # id comment",
                "id = \"desk-2\"  # id comment"
            )
        );
        for to in ["second", "SECOND"] {
            let e = err(
                DOC,
                &[ConfigEdit::RenameAccount {
                    from: "desk".into(),
                    to: to.into(),
                }],
            );
            assert!(e.message.contains("already"), "{e}");
        }
        let e = err(
            DOC,
            &[ConfigEdit::RenameAccount {
                from: "desk".into(),
                to: "a b".into(),
            }],
        );
        assert_eq!(e.path, "account.id");
        // Renaming to the same id is allowed.
        run(
            DOC,
            &[ConfigEdit::RenameAccount {
                from: "desk".into(),
                to: "desk".into(),
            }],
        );
    }

    fn blf(uri: &str, label: &str) -> BlfConfig {
        BlfConfig {
            uri: uri.into(),
            label: label.into(),
            ..BlfConfig::default()
        }
    }

    #[test]
    fn set_blf_writes_group_and_pickup_and_drops_them_when_cleared() {
        let mut key = blf("1002", "Boss");
        key.group = "Sales".into();
        key.pickup = OrInherit::Value(PickupMode::Prefix);
        key.pickup_prefix = "**".into();
        key.pickup_target = "8001".into();
        let out = run(
            DOC,
            &[ConfigEdit::SetBlf {
                account: "desk".into(),
                entries: vec![key],
            }],
        );
        let cfg = Config::parse(&out).unwrap();
        let b = &cfg.accounts[0].blf[0];
        assert_eq!(b.group, "Sales");
        assert_eq!(b.pickup, OrInherit::Value(PickupMode::Prefix));
        assert_eq!(
            (b.pickup_prefix.as_str(), b.pickup_target.as_str()),
            ("**", "8001")
        );
        // Back to inherit: the optional keys leave the file, the entry's comment stays.
        let out = run(
            &out,
            &[ConfigEdit::SetBlf {
                account: "desk".into(),
                entries: vec![blf("1002", "Boss")],
            }],
        );
        assert!(!out.contains("group") && !out.contains("pickup"), "{out}");
        assert!(out.contains("# boss ext"), "{out}");
    }

    #[test]
    fn account_pickup_fields_and_resume_after_failed_transfer_are_editable() {
        let out = run(
            DOC,
            &[
                ConfigEdit::SetAccount {
                    id: "desk".into(),
                    field: AccountField::Pickup,
                    value: EditValue::Str("prefix".into()),
                },
                ConfigEdit::SetAccount {
                    id: "desk".into(),
                    field: AccountField::PickupPrefix,
                    value: EditValue::Str("*8".into()),
                },
                ConfigEdit::SetSection {
                    section: "calls".into(),
                    key: "resume_after_failed_transfer".into(),
                    value: EditValue::Bool(false),
                },
            ],
        );
        let cfg = Config::parse(&out).unwrap();
        assert_eq!(cfg.accounts[0].pickup, PickupMode::Prefix);
        assert_eq!(cfg.accounts[0].pickup_prefix, "*8");
        assert!(!cfg.calls.resume_after_failed_transfer);
    }

    #[test]
    fn set_blf_keeps_the_decor_of_matched_entries() {
        // Reorder, relabel one, drop none, add one.
        let out = run(
            DOC,
            &[ConfigEdit::SetBlf {
                account: "desk".into(),
                entries: vec![
                    blf("1003", "Carl"),
                    blf("1002", "The Boss"),
                    blf("1004", "Dee"),
                ],
            }],
        );
        let cfg = Config::parse(&out).unwrap();
        let uris: Vec<_> = cfg.accounts[0].blf.iter().map(|b| b.uri.as_str()).collect();
        assert_eq!(uris, ["1003", "1002", "1004"]);
        assert_eq!(cfg.accounts[0].blf[1].label, "The Boss");
        assert!(out.contains("uri = \"1002\"   # boss ext"), "{out}");
        assert!(out.contains("# BLF: boss"), "{out}");
        assert!(out.contains("# --- Second ---") && cfg.accounts[1].id == "second");
        // Shrinking drops the unmatched ones; empty removes the key.
        let out = run(
            DOC,
            &[ConfigEdit::SetBlf {
                account: "desk".into(),
                entries: vec![blf("1002", "Boss")],
            }],
        );
        assert!(!out.contains("Carl") && out.contains("# boss ext"));
        let out = run(
            DOC,
            &[ConfigEdit::SetBlf {
                account: "desk".into(),
                entries: vec![],
            }],
        );
        assert!(!out.contains("blf") && !out.contains("Carl"), "{out}");
        // An account without any, gains some.
        let out = run(
            DOC,
            &[ConfigEdit::SetBlf {
                account: "second".into(),
                entries: vec![blf("2002", "")],
            }],
        );
        let cfg = Config::parse(&out).unwrap();
        assert_eq!(cfg.accounts[1].blf, [blf("2002", "")]);
        assert!(
            !out.contains("label = \"\""),
            "default label not written: {out}"
        );
    }

    #[test]
    fn sections_are_created_appended_and_updated() {
        let out = run(
            DOC,
            &[
                sec("ui", "theme", "dark"),
                sec("logging", "redact_credentials", true),
            ],
        );
        assert!(out.starts_with(DOC), "{out}");
        assert!(out.contains("[ui]\ntheme = \"dark\"\n"), "{out}");
        let cfg = Config::parse(&out).unwrap();
        assert_eq!(cfg.ui.theme, crate::Theme::Dark);
        assert!(cfg.logging.redact_credentials);
        // Existing keys update in place with their decor.
        let out = run(
            DOC,
            &[
                sec("calls", "hold_scope", "account"),
                sec("general", "log_level", "debug"),
            ],
        );
        assert!(
            out.contains("hold_scope = \"account\"  # hold comment"),
            "{out}"
        );
        assert!(out.contains("log_level = \"debug\"   # keep this"), "{out}");
        // Shortcuts live in [ui.shortcuts].
        let out = run(DOC, &[sec("ui", "shortcuts.answer", "Mod+Shift+A")]);
        let cfg = Config::parse(&out).unwrap();
        assert_eq!(
            cfg.ui.shortcuts.get("answer").map(String::as_str),
            Some("Mod+Shift+A")
        );
        let out2 = run(
            &out,
            &[
                sec("ui", "shortcuts.answer", "Mod+A"),
                sec("ui", "shortcuts.hangup", "Mod+H"),
            ],
        );
        assert_eq!(Config::parse(&out2).unwrap().ui.shortcuts.len(), 2);
        let out3 = run(
            &out2,
            &[ConfigEdit::ResetSection {
                section: "ui".into(),
                key: "shortcuts.answer".into(),
            }],
        );
        assert_eq!(Config::parse(&out3).unwrap().ui.shortcuts.len(), 1);
    }

    #[test]
    fn a_bad_shortcut_entry_can_be_reset_but_not_set() {
        let broken = "[ui.shortcuts]\nteleport = \"Mod+T\"\n";
        let reset = ConfigEdit::ResetSection {
            section: "ui".into(),
            key: "shortcuts.teleport".into(),
        };
        let out = run(broken, std::slice::from_ref(&reset));
        assert!(Config::parse(&out).unwrap().ui.shortcuts.is_empty());
        // Removing an entry that is not there is fine too.
        run("", &[reset]);
        let set = sec("ui", "shortcuts.teleport", "Mod+T");
        let mut d = doc("");
        assert!(apply_edits(&mut d, &[set]).is_err());
    }

    #[test]
    fn unknown_or_file_only_sections_and_keys_are_rejected() {
        for (s, k, p) in [
            ("control", "tcp_listen", "control"),
            ("media", "codecs", "media"),
            ("speech", "tts_command", "speech"),
            ("nope", "x", "nope"),
            ("ui", "nope", "ui.nope"),
            (
                "calls",
                "resume_after_nothing",
                "calls.resume_after_nothing",
            ),
            ("ui", "shortcuts.", "ui.shortcuts."),
        ] {
            let e = err(DOC, &[sec(s, k, true)]);
            assert_eq!(e.path, p, "{e}");
        }
        let e = err(
            DOC,
            &[ConfigEdit::ResetSection {
                section: "media".into(),
                key: "x".into(),
            }],
        );
        assert_eq!(e.path, "media");
    }

    #[test]
    fn reset_section_removes_the_key_and_keeps_the_rest() {
        let out = run(
            DOC,
            &[ConfigEdit::ResetSection {
                section: "calls".into(),
                key: "hold_scope".into(),
            }],
        );
        assert!(
            out.contains("[calls]\n") && !out.contains("hold_scope"),
            "{out}"
        );
        assert!(out.contains("# Calls section"));
    }

    fn acct_doc(extra_after_a: &str) -> String {
        format!(
            "# file header\n\n# --- a ---\n[[account]]\nid = \"a\"\nusername = \"1\"\ndomain = \"d\"\n{extra_after_a}\n\
             # --- b ---\n[[account]]\nid = \"b\"\nusername = \"2\"\ndomain = \"d\"\n# trailing note about b\n\n\
             # --- c ---\n[[account]]\nid = \"c\"\nusername = \"3\"\ndomain = \"d\"\n"
        )
    }

    #[test]
    fn removals_drop_only_the_comment_block_above_the_item() {
        // Middle account: the previous account's trailing comments stay, its own header comment goes.
        let d = acct_doc("# trailing note about a\n");
        let out = run(&d, &[ConfigEdit::RemoveAccount { id: "b".into() }]);
        assert!(out.contains("# trailing note about a"), "{out}");
        assert!(
            !out.contains("--- b ---") && !out.contains("id = \"b\""),
            "{out}"
        );
        // b's trailing note belongs to c's prefix region: it is before c's block, kept.
        assert!(
            out.contains("# trailing note about b") && out.contains("# --- c ---"),
            "{out}"
        );
        // First table: the file-header comment stays.
        let out = run(&d, &[ConfigEdit::RemoveAccount { id: "a".into() }]);
        assert!(out.starts_with("# file header\n"), "{out}");
        assert!(
            !out.contains("--- a ---") && out.contains("--- b ---"),
            "{out}"
        );
        // Last table: its note-for-the-previous survives at the end of the file.
        let out = run(&d, &[ConfigEdit::RemoveAccount { id: "c".into() }]);
        assert!(
            out.contains("# trailing note about b") && !out.contains("--- c ---"),
            "{out}"
        );
        // Only account: header kept.
        let one = "# file header\n\n# --- a ---\n[[account]]\nid = \"a\"\nusername = \"1\"\ndomain = \"d\"\n";
        let out = run(one, &[ConfigEdit::RemoveAccount { id: "a".into() }]);
        assert_eq!(out, "# file header\n\n");
    }

    #[test]
    fn replace_example_keeps_the_previous_sections_trailing_note() {
        let shipped = crate::EXAMPLE_CONFIG.replace(
            "stt_language = \"en\"",
            "stt_language = \"en\"\n# trailing speech note",
        );
        let out = run(
            &shipped,
            &[ConfigEdit::AddAccount {
                draft: draft(
                    "mine",
                    &[
                        (AccountField::Username, "1".into()),
                        (AccountField::Domain, "d".into()),
                    ],
                ),
                replace_example: true,
            }],
        );
        assert!(out.contains("# trailing speech note"), "{out}");
        assert!(!out.contains("Sample account"), "{out}");
        assert_eq!(Config::parse(&out).unwrap().accounts[0].id, "mine");
    }

    #[test]
    fn reset_keeps_the_comments_above_the_key() {
        let shipped = crate::EXAMPLE_CONFIG;
        let reset = |section: &str, key: &str| {
            run(
                shipped,
                &[ConfigEdit::ResetSection {
                    section: section.into(),
                    key: key.into(),
                }],
            )
        };
        let out = reset("general", "log_level");
        assert!(out.contains("# user_agent = \"Siple/<version>\""), "{out}");
        assert!(!out.contains("\nlog_level = "), "{out}");
        let out = reset("calls", "hold_scope");
        assert!(out.contains("# Which calls get put on hold"), "{out}");
        assert!(out.contains("# Hot reloadable."), "{out}");
        assert!(!out.contains("\nhold_scope = "), "{out}");
        // The last key of a table: its comment moves to the end of the previous key's line.
        let t = "[general]\nlog_level = \"info\"\n# about user_agent\nuser_agent = \"x\"\n";
        let out = run(
            t,
            &[ConfigEdit::ResetSection {
                section: "general".into(),
                key: "user_agent".into(),
            }],
        );
        assert!(
            out.contains("# about user_agent") && !out.contains("user_agent ="),
            "{out}"
        );
        // Account keys likewise.
        let a = "[[account]]\nid = \"a\"\n# the name\ndisplay_name = \"Z\"\nusername = \"1\"\ndomain = \"d\"\n";
        let out = run(
            a,
            &[ConfigEdit::ResetAccount {
                id: "a".into(),
                field: AccountField::DisplayName,
            }],
        );
        let cfg = Config::parse(&out).unwrap();
        assert!(
            cfg.accounts[0].display_name.is_empty() && out.contains("# the name"),
            "{out}"
        );
    }

    #[test]
    fn dropped_blf_entries_leave_the_neighbours_comments() {
        let d = "[[account]]\nid = \"a\"\nusername = \"1\"\ndomain = \"d\"\n# about the blf list\n\n\
                 # boss\n[[account.blf]]\nuri = \"1\"\n\n# carl\n[[account.blf]]\nuri = \"2\"\n";
        let out = run(
            d,
            &[ConfigEdit::SetBlf {
                account: "a".into(),
                entries: vec![blf("2", "")],
            }],
        );
        assert!(
            out.contains("# about the blf list") && out.contains("# carl"),
            "{out}"
        );
        assert!(
            !out.contains("# boss") && !out.contains("uri = \"1\""),
            "{out}"
        );
        let out = run(
            d,
            &[ConfigEdit::SetBlf {
                account: "a".into(),
                entries: vec![],
            }],
        );
        assert!(
            out.contains("# about the blf list") && !out.contains("[[account.blf]]"),
            "{out}"
        );
    }

    #[test]
    fn kept_comments_stay_in_file_order() {
        // Removing the last account while another section follows: the previous account's
        // trailing note must not drift below the `[ui]` section.
        let d = format!("{}\n[ui]\ntheme = \"dark\"\n", acct_doc(""));
        let out = run(&d, &[ConfigEdit::RemoveAccount { id: "c".into() }]);
        let note = out.find("# trailing note about b").unwrap();
        assert!(out.find("id = \"b\"").unwrap() < note, "{out}");
        assert!(note < out.find("[ui]").unwrap(), "{out}");
        // An emptied BLF list: the comment about it stays with its account, not the next one.
        let d = "[[account]]\nid = \"a\"\nusername = \"1\"\ndomain = \"d\"\n# trailing note on a\n\n\
                 [[account.blf]]\nuri = \"1\"\n\n[[account]]\nid = \"b\"\nusername = \"2\"\ndomain = \"d\"\n";
        let out = run(
            d,
            &[ConfigEdit::SetBlf {
                account: "a".into(),
                entries: vec![],
            }],
        );
        let at = out.find("# trailing note on a").unwrap();
        assert!(out.find("domain = \"d\"").unwrap() < at, "{out}");
        assert!(at < out.find("id = \"b\"").unwrap(), "{out}");
        assert_eq!(Config::parse(&out).unwrap().accounts.len(), 2);
        // First account removed with sections before the accounts: the note goes below them.
        let d = "[ui]\ntheme = \"dark\"\n# after ui\n\n# --- a ---\n[[account]]\nid = \"a\"\n\
                 username = \"1\"\ndomain = \"d\"\n";
        let out = run(d, &[ConfigEdit::RemoveAccount { id: "a".into() }]);
        assert!(
            out.contains("# after ui") && !out.contains("--- a ---"),
            "{out}"
        );
    }

    #[test]
    fn text_separated_by_a_blank_line_is_never_deleted() {
        // `# note about a` sits above a blank line, then b's header: it may be a's note, so it
        // stays when b is removed. Only the block directly above b goes.
        let d = "[[account]]\nid = \"a\"\nusername = \"1\"\ndomain = \"d\"\n# note about a\n\n\
                 # --- b ---\n[[account]]\nid = \"b\"\nusername = \"2\"\ndomain = \"d\"\n\n\
                 # --- c ---\n[[account]]\nid = \"c\"\nusername = \"3\"\ndomain = \"d\"\n";
        let out = run(d, &[ConfigEdit::RemoveAccount { id: "b".into() }]);
        assert!(out.contains("# note about a"), "{out}");
        assert!(
            !out.contains("--- b ---") && out.contains("# --- c ---"),
            "{out}"
        );
        assert!(out.find("# note about a").unwrap() < out.find("id = \"c\"").unwrap());
        // A banner that is itself separated from the header by a blank line is also kept.
        let d = "[[account]]\nid = \"a\"\nusername = \"1\"\ndomain = \"d\"\n\n# --- b banner ---\n\n\
                 [[account]]\nid = \"b\"\nusername = \"2\"\ndomain = \"d\"\n";
        let out = run(d, &[ConfigEdit::RemoveAccount { id: "b".into() }]);
        assert!(out.contains("# --- b banner ---"), "{out}");
        assert_eq!(
            split_prefix("# keep\n\n# own\n"),
            ("# keep\n\n".into(), "# own\n".into())
        );
        assert_eq!(
            split_prefix("# a\n\n# b\n\n"),
            ("# a\n\n# b\n\n".into(), String::new())
        );
        assert_eq!(split_prefix("\n# own\n"), ("\n".into(), "# own\n".into()));
    }

    #[test]
    fn removing_a_key_without_neighbouring_values_keeps_its_comment() {
        // The only key of a table.
        let t = "[ui.shortcuts]\n# about answer\nanswer = \"x\"\n";
        let out = run(
            t,
            &[ConfigEdit::ResetSection {
                section: "ui".into(),
                key: "shortcuts.answer".into(),
            }],
        );
        assert!(
            out.contains("# about answer") && !out.contains("answer ="),
            "{out}"
        );
        // Both [calls] keys reset in one batch: the explanation survives.
        let reset = |key: &str| ConfigEdit::ResetSection {
            section: "calls".into(),
            key: key.into(),
        };
        let out = run(
            crate::EXAMPLE_CONFIG,
            &[reset("hold_scope"), reset("decline_code")],
        );
        assert!(out.contains("# Which calls get put on hold"), "{out}");
        assert!(out.contains("# Hot reloadable."), "{out}");
        assert!(
            !out.contains("\nhold_scope =") && !out.contains("\ndecline_code ="),
            "{out}"
        );
        assert!(out.contains("[ui]"), "{out}");
        // Reset then set the same key: its doc comment goes back above it, not above the
        // neighbor.
        let out = run(
            crate::EXAMPLE_CONFIG,
            &[reset("hold_scope"), sec("calls", "hold_scope", "account")],
        );
        assert!(
            out.contains("# Hot reloadable.\nhold_scope = \"account\""),
            "{out}"
        );
        assert!(!out.contains("# Hot reloadable.\ndecline_code"), "{out}");
    }

    #[test]
    fn general_log_keys_are_file_only() {
        for key in ["log_dir", "log_retention_days", "reconfig_force"] {
            let e = err(DOC, &[sec("general", key, true)]);
            assert_eq!(e.path, format!("general.{key}"));
        }
        run(DOC, &[sec("general", "log_level", "debug")]);
    }

    #[test]
    fn edits_json_shape() {
        let v: Vec<ConfigEdit> = serde_json::from_str(
            r#"[
              {"op":"set_account","id":"a","field":"username","value":"1"},
              {"op":"reset_account","id":"a","field":"dnd"},
              {"op":"add_account","draft":{"id":"n","fields":{"username":"2","local_port":5070}},"replace_example":true},
              {"op":"remove_account","id":"a"},
              {"op":"move_account","id":"a","index":2},
              {"op":"rename_account","from":"a","to":"b"},
              {"op":"set_blf","account":"a","entries":[{"uri":"1","label":"x"}]},
              {"op":"set_section","section":"ui","key":"tray","value":false},
              {"op":"reset_section","section":"ui","key":"tray"}
            ]"#,
        )
        .unwrap();
        assert_eq!(v.len(), 9);
        let back: Vec<ConfigEdit> =
            serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
        assert_eq!(v, back);
        for bad in [
            r#"{"op":"set_account","id":"a","field":"extra_headers","value":"x"}"#,
            r#"{"op":"set_account","id":"a","field":"dnd","value":null}"#,
            r#"{"op":"set_account","id":"a","field":"dnd","value":1.5}"#,
            r#"{"op":"set_account","id":"a","field":"dnd","value":true,"extra":1}"#,
            r#"{"op":"explode"}"#,
        ] {
            assert!(serde_json::from_str::<ConfigEdit>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn every_account_field_round_trips_through_the_schema() {
        // Each field accepts a valid value of its kind and the result parses.
        let sample = |f: AccountField| -> EditValue {
            match f.kind() {
                Kind::Bool => EditValue::Bool(true),
                Kind::Int(lo, hi) => EditValue::Int(if lo == 0 { hi.min(1234) } else { lo }),
                Kind::Str => EditValue::Str(match f {
                    AccountField::TlsFingerprint => "ab".repeat(32),
                    _ => "x".into(),
                }),
                Kind::Enum(options) => EditValue::Str(options[options.len() - 1].into()),
            }
        };
        for f in AccountField::ALL {
            let mut d = doc(DOC);
            // Flip the value away from the default so it is written.
            let defaults = toml::Value::try_from(AccountConfig::default()).unwrap();
            let v = match sample(f) {
                EditValue::Bool(_) => EditValue::Bool(
                    !defaults
                        .get(f.key())
                        .and_then(toml::Value::as_bool)
                        .unwrap(),
                ),
                v => v,
            };
            apply_edits(&mut d, &[set("second", f, v)]).unwrap();
            let cfg = Config::parse(&d.to_string()).unwrap_or_else(|e| panic!("{f:?}: {e}"));
            assert_eq!(cfg.accounts.len(), 2);
            assert!(
                d.to_string().contains(&format!("\n{} = ", f.key())),
                "{f:?} written: {d}"
            );
        }
        let toml = toml::Value::try_from(AccountConfig::default()).unwrap();
        for f in AccountField::ALL {
            assert!(toml.get(f.key()).is_some(), "{f:?} is a schema key");
        }
    }

    #[test]
    fn section_table_covers_the_schema_and_enum_values_parse() {
        let defaults = toml::Value::try_from(Config::default()).unwrap();
        for (section, keys) in SECTIONS {
            let table = defaults
                .get(*section)
                .and_then(toml::Value::as_table)
                .unwrap();
            // Not editable on purpose: see SECTIONS.
            let file_only = [
                "shortcuts",
                "log_dir",
                "log_retention_days",
                "reconfig_force",
            ];
            for (key, _) in table
                .iter()
                .filter(|(k, _)| !file_only.contains(&k.as_str()))
            {
                assert!(
                    keys.iter().any(|(k, _)| k == key),
                    "{section}.{key} is in the schema but not editable (add it to SECTIONS)"
                );
            }
            // Optional keys have no default value, so the serialized defaults omit them.
            let optional = [("updates", "channel")];
            for (key, kind) in *keys {
                assert!(
                    table.contains_key(*key) || optional.contains(&(*section, *key)),
                    "{section}.{key} is not in the schema"
                );
                // Every offered enum value must be accepted by the real parser.
                if let Kind::Enum(options) = kind {
                    for o in *options {
                        apply_edits(&mut doc(""), &[sec(section, key, *o)]).unwrap();
                        let text = format!("[{section}]\n{key} = \"{o}\"\n");
                        Config::parse(&text).unwrap_or_else(|e| panic!("{section}.{key}={o}: {e}"));
                    }
                }
            }
        }
        // Account enums, too.
        for (f, options) in [
            (AccountField::Profile, PROFILES),
            (AccountField::Transport, TRANSPORTS),
            (AccountField::Srtp, SRTP),
            (AccountField::Mwi, MWI),
        ] {
            for o in options {
                let t = format!("[[account]]\nid = \"a\"\n{} = \"{o}\"\n", f.key());
                Config::parse(&t).unwrap_or_else(|e| panic!("{f:?}={o}: {e}"));
            }
        }
    }
}
