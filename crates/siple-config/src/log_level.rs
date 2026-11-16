//! `[general].log_level` interpretation, shared by validation and the logging setup.

use tracing_subscriber::filter::EnvFilter;

/// Filter used when nothing is configured.
pub const DEFAULT_FILTER: &str = "info,rsipstack=warn";

/// Turns the `log_level` config value into an `EnvFilter` string: a bare level gets
/// `rsipstack=warn` appended, anything else is used verbatim.
pub fn effective_filter(log_level: &str) -> String {
    let level = log_level.trim();
    if level.is_empty() {
        DEFAULT_FILTER.to_string()
    } else if is_level_name(level) {
        format!("{},rsipstack=warn", level.to_ascii_lowercase())
    } else {
        // EnvFilter rejects whitespace around directives; normalize it away.
        level
            .split(',')
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn is_level_name(s: &str) -> bool {
    ["trace", "debug", "info", "warn", "error", "off"]
        .iter()
        .any(|l| s.eq_ignore_ascii_case(l))
}

/// Validates a `[general].log_level` value: empty (default), or a comma-separated list of
/// directives that are each a level name or contain `=` (`target=level`), and the whole thing
/// must parse as an `EnvFilter`. A bare word such as `inf` would otherwise be read as a
/// *target* and silently disable all logging.
pub fn validate_log_level(log_level: &str) -> Result<(), String> {
    let level = log_level.trim();
    if level.is_empty() {
        return Ok(());
    }
    for directive in level.split(',').map(str::trim) {
        if directive.is_empty() {
            return Err("empty directive (stray comma)".into());
        }
        if !directive.contains('=') && !is_level_name(directive) {
            return Err(format!(
                "{directive:?} is neither a level (trace|debug|info|warn|error|off) nor a target=level directive"
            ));
        }
    }
    EnvFilter::try_new(effective_filter(level))
        .map(|_| ())
        .map_err(|e| format!("invalid filter: {e}"))
}
