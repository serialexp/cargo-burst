//! Post-mortem detectors that scan a failed cargo run's combined
//! output and translate common pitfalls into actionable hints.
//!
//! Right now there's exactly one class of detector: missing
//! environment variables. cargo-burst doesn't forward the user's env
//! to the remote by default (see `forward_env` in the README), so
//! tests that assume e.g. `DATABASE_URL` is set will compile but
//! panic at runtime with an `environment variable not found` error.
//! The user has to know about `--env` and the connection-string
//! conventions to fix it; this module spots the panic and points
//! them there.
//!
//! The detector is deliberately conservative: it only fires on
//! patterns that very specifically indicate a missing-env failure,
//! not generic "test failed" panics that happened to mention an env
//! var name in passing. False positives are worse than false
//! negatives — a wrong hint trains the user to ignore them.

/// A missing-env-var failure that a known cargo-burst feature can
/// fix, plus a suggested concrete value if the var is one of the
/// canned database services baked into the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvHint {
    /// The environment variable name detected as missing.
    pub var: String,
    /// A connection string that would work on the burst remote, if
    /// `var` is one of the well-known DB-related names. Otherwise
    /// `None` and the user is asked to provide their own value.
    pub suggested_value: Option<String>,
}

/// Map of environment-variable names to the connection string that
/// hits the matching service preinstalled in the burst image. Kept
/// in sync with the conventions documented in `bake-image.sh` and
/// the README. Order doesn't matter — names are matched by exact
/// equality.
const KNOWN_VARS: &[(&str, &str)] = &[
    ("DATABASE_URL", "postgres://postgres@localhost:5432/postgres"),
    ("POSTGRES_URL", "postgres://postgres@localhost:5432/postgres"),
    ("POSTGRESQL_URL", "postgres://postgres@localhost:5432/postgres"),
    ("MYSQL_URL", "mysql://root:root@localhost:3306/"),
    ("REDIS_URL", "redis://localhost:6379/"),
];

/// Scan `output` (combined remote stdout+stderr) for evidence that a
/// run failed because some env var wasn't set on the remote. Returns
/// the first hit, or `None` if nothing matches.
///
/// We try the known-var list first for higher confidence; the
/// generic fallback then catches arbitrary names that the user
/// would have forwarded with `--env NAME=value`.
pub fn detect_env_hint(output: &str) -> Option<EnvHint> {
    for (name, suggested) in KNOWN_VARS {
        if missing_env_evidence(output, name) {
            return Some(EnvHint {
                var: (*name).to_string(),
                suggested_value: Some((*suggested).to_string()),
            });
        }
    }
    detect_generic_missing_env(output).map(|name| EnvHint { var: name, suggested_value: None })
}

/// Return true if `output` contains a phrase that very specifically
/// indicates `name` was looked up via `std::env::var` (or a
/// SQLx-style compile-time macro) and was missing. Anchoring on these
/// exact phrases avoids matching the var name when it appears in,
/// say, a successful "DATABASE_URL=…" log line.
fn missing_env_evidence(output: &str, name: &str) -> bool {
    let patterns = [
        // std::env::var: `Err(NotPresent)` formatted via Debug:
        //   "DATABASE_URL": environment variable not found
        format!("\"{name}\": environment variable not found"),
        // dotenv, diesel, ad-hoc panic patterns:
        //   DATABASE_URL must be set
        format!("{name} must be set"),
        // SQLx / diesel error messages with backticked or quoted names:
        format!("environment variable `{name}` not found"),
        format!("environment variable `{name}` is not defined"),
        format!("environment variable '{name}' is not defined"),
        format!("environment variable '{name}' not found"),
        // Some panics phrase it as `<NAME> is not set`:
        format!("`{name}` is not set"),
        format!("'{name}' is not set"),
    ];
    patterns.iter().any(|p| output.contains(p.as_str()))
}

/// Generic fallback: extract the variable name from the most common
/// "environment variable X (not found|is not defined|must be set)"
/// shapes, when the name isn't in our known list. We don't bring in
/// `regex` for this — the patterns are tightly enough constrained
/// that a small custom scanner is clearer than a regex would be.
///
/// Returns the env-var name (uppercase letters/digits/underscores)
/// or `None` when no anchor matches.
fn detect_generic_missing_env(output: &str) -> Option<String> {
    // Anchor phrases. The position immediately after `idx + anchor.len()`
    // — or before, depending on the shape — is the var-name capture
    // site. We try each anchor and the first valid extracted name wins.
    //
    // For "X must be set" the name comes BEFORE the anchor, so we
    // reverse-scan. For "environment variable `X` not found" etc.,
    // the name is INSIDE the quotes/backticks immediately after the
    // anchor. We handle both shapes.
    if let Some(name) = scan_quoted_after(output, "environment variable `", "`") {
        return Some(name);
    }
    if let Some(name) = scan_quoted_after(output, "environment variable '", "'") {
        return Some(name);
    }
    if let Some(name) = scan_quoted_after(output, "environment variable \"", "\"") {
        return Some(name);
    }
    // "<NAME> must be set" — uncommon to false-positive on because
    // the entire phrase is distinctive.
    if let Some(name) = scan_word_before(output, " must be set") {
        return Some(name);
    }
    None
}

/// After the first occurrence of `prefix`, capture characters up to
/// `suffix` and return them if they look like an env-var name
/// (uppercase letters, digits, underscores; first char alphabetic).
/// Returns `None` if the capture doesn't match that shape — protects
/// against matching a stray sentence that happens to begin with the
/// anchor.
fn scan_quoted_after(haystack: &str, prefix: &str, suffix: &str) -> Option<String> {
    let start = haystack.find(prefix)? + prefix.len();
    let rest = &haystack[start..];
    let end = rest.find(suffix)?;
    let candidate = &rest[..end];
    if !looks_like_env_name(candidate) {
        return None;
    }
    // Confirm the surrounding context is actually a missing-env
    // phrase (not just any sentence containing "environment variable
    // `X`"). Any of these tail phrases inside the next ~80 chars
    // qualifies.
    let tail = &rest[end..];
    let tail_window = &tail[..tail.len().min(80)];
    let qualifies = tail_window.contains("not found")
        || tail_window.contains("not defined")
        || tail_window.contains("must be set")
        || tail_window.contains("is not set");
    if !qualifies {
        return None;
    }
    Some(candidate.to_string())
}

/// Walk backwards from the start of `marker` (e.g. " must be set") to
/// extract the env-var-shaped word that precedes it. Skips over any
/// trailing whitespace before the marker (in practice there is none
/// because the marker starts with a space).
fn scan_word_before(haystack: &str, marker: &str) -> Option<String> {
    let idx = haystack.find(marker)?;
    let before = &haystack[..idx];
    // Walk back over a contiguous run of [A-Z0-9_].
    let bytes = before.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        let b = bytes[i - 1];
        let is_envchar = b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_';
        if !is_envchar {
            break;
        }
        i -= 1;
    }
    let candidate = &before[i..];
    if !looks_like_env_name(candidate) {
        return None;
    }
    Some(candidate.to_string())
}

/// Heuristic for "is this string plausibly an env-var name?"
/// `[A-Z][A-Z0-9_]+` with length 2..=64. Lower bound rules out
/// single-letter false positives; upper bound rules out runaway
/// captures across pathological output.
fn looks_like_env_name(s: &str) -> bool {
    if s.len() < 2 || s.len() > 64 {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_uppercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

impl EnvHint {
    /// Pretty-printed multi-line hint message, suitable for emission
    /// to stderr after a failed run. The leading blank line lets it
    /// stand off from cargo's own output.
    pub fn format(&self, verb: &str) -> String {
        let var = &self.var;
        let mut out = String::new();
        out.push('\n');
        out.push_str("\u{1f4a1} Hint: the failure looks like a missing environment ");
        out.push_str(&format!("variable ({var}) on the remote.\n"));
        out.push_str(
            "   cargo-burst doesn't forward your local env to the remote unless you ask it to.\n",
        );
        if let Some(value) = &self.suggested_value {
            out.push_str(&format!(
                "   For the burst image's preinstalled service, run:\n     cargo burst {verb} --env {var}={value}\n"
            ));
            out.push_str(&format!(
                "   Or to forward whatever {var} is set to in your shell:\n     cargo burst {verb} --env {var}\n"
            ));
        } else {
            out.push_str(&format!(
                "   To forward your local value:\n     cargo burst {verb} --env {var}\n"
            ));
            out.push_str(&format!(
                "   Or to set it inline:\n     cargo burst {verb} --env {var}=<value>\n"
            ));
        }
        out.push_str(
            "   Persist via the `forward_env` field in config.toml (global) or\n   <workspace>/.config/cargo-burst.toml (project). See `cargo burst --help`.\n",
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_database_url_env_var_not_found_panic() {
        let out = r#"thread 'tests::it_works' panicked at src/lib.rs:42:14:
called `Result::unwrap()` on an `Err` value: NotPresent
"DATABASE_URL": environment variable not found"#;
        let hint = detect_env_hint(out).unwrap();
        assert_eq!(hint.var, "DATABASE_URL");
        assert_eq!(
            hint.suggested_value.as_deref(),
            Some("postgres://postgres@localhost:5432/postgres")
        );
    }

    #[test]
    fn detects_database_url_must_be_set() {
        let out = "thread 'main' panicked at src/main.rs:5:5:\nDATABASE_URL must be set\n";
        let hint = detect_env_hint(out).unwrap();
        assert_eq!(hint.var, "DATABASE_URL");
    }

    #[test]
    fn detects_redis_url() {
        let out = "REDIS_URL must be set";
        let hint = detect_env_hint(out).unwrap();
        assert_eq!(hint.var, "REDIS_URL");
        assert_eq!(hint.suggested_value.as_deref(), Some("redis://localhost:6379/"));
    }

    #[test]
    fn detects_sqlx_compile_time_message() {
        let out = "error: environment variable `DATABASE_URL` not found";
        let hint = detect_env_hint(out).unwrap();
        assert_eq!(hint.var, "DATABASE_URL");
    }

    #[test]
    fn detects_generic_unknown_var_via_quoted_phrase() {
        let out = "error: environment variable `MY_API_TOKEN` is not defined";
        let hint = detect_env_hint(out).unwrap();
        assert_eq!(hint.var, "MY_API_TOKEN");
        assert!(hint.suggested_value.is_none());
    }

    #[test]
    fn detects_generic_unknown_var_via_must_be_set() {
        let out = "thread 'main' panicked at src/main.rs:5:5:\nGRAFANA_API_KEY must be set\n";
        let hint = detect_env_hint(out).unwrap();
        assert_eq!(hint.var, "GRAFANA_API_KEY");
        assert!(hint.suggested_value.is_none());
    }

    #[test]
    fn rejects_unrelated_failures() {
        // No env-var anchor anywhere — must not falsely fire.
        let out = "test failed: assertion failed: left == right\n  left: 1\n  right: 2";
        assert!(detect_env_hint(out).is_none());
    }

    #[test]
    fn rejects_var_name_in_passing_mention() {
        // Mentions DATABASE_URL but in a successful log line, not a
        // missing-env panic. Should not fire.
        let out = "Successfully connected to DATABASE_URL=postgres://...\nAll tests passed.";
        assert!(detect_env_hint(out).is_none());
    }

    #[test]
    fn rejects_lowercase_or_short_garbage() {
        // The phrase exists but the captured name is junk.
        let out = "environment variable `x` not found";
        assert!(detect_env_hint(out).is_none());
        let out = "environment variable `lowercase_thing` not found";
        assert!(detect_env_hint(out).is_none());
    }

    #[test]
    fn looks_like_env_name_boundaries() {
        assert!(!looks_like_env_name(""));
        assert!(!looks_like_env_name("X"));        // too short
        assert!(!looks_like_env_name("xY"));       // first char lowercase
        assert!(looks_like_env_name("XY"));
        assert!(looks_like_env_name("DATABASE_URL"));
        assert!(looks_like_env_name("FOO123_BAR"));
        assert!(!looks_like_env_name("DASH-NAME"));
        assert!(!looks_like_env_name(&"A".repeat(65))); // upper bound
    }

    #[test]
    fn format_named_var_includes_both_explicit_and_forward_recipes() {
        let h = EnvHint {
            var: "DATABASE_URL".into(),
            suggested_value: Some("postgres://postgres@localhost:5432/postgres".into()),
        };
        let s = h.format("test");
        assert!(s.contains("--env DATABASE_URL=postgres://postgres@localhost:5432/postgres"));
        assert!(s.contains("--env DATABASE_URL\n"));
        assert!(s.contains("cargo burst test"));
    }

    #[test]
    fn format_unknown_var_omits_suggested_value() {
        let h = EnvHint { var: "MY_TOKEN".into(), suggested_value: None };
        let s = h.format("test");
        assert!(s.contains("--env MY_TOKEN"));
        assert!(!s.contains("MY_TOKEN=postgres"));
    }
}
