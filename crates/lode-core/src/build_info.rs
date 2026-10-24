//! What this lode build is: version, commit and supported features.
//!
//! Handed to the app through `LODE_VERSION` / `LODE_COMMIT` / `LODE_FEATURES`
//! and `state.json`'s `lode` object.

/// The lode version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The full git commit hash, when the build knew it (`LODE_BUILD_COMMIT`, or
/// the lode checkout it was built in).
pub const COMMIT: Option<&str> = option_env!("LODE_BUILD_COMMIT");

/// `VERSION`, followed by the short commit when known: `0.3.0 (0123456789ab)`.
pub const LONG_VERSION: &str = env!("LODE_BUILD_LONG_VERSION");

/// App-contract features this lode supports. Names are stable: new ones are
/// appended, none is ever removed or renamed.
pub const FEATURES: &[&str] = &[
    "target",
    "restart",
    "check",
    "hold",
    "readiness-state",
    "staged-cutover",
    "config-generation",
    "resources",
    "resource-target",
    "report",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_version_names_the_version_and_short_commit() {
        match COMMIT {
            Some(commit) => {
                assert!(commit.chars().all(|c| c.is_ascii_hexdigit()), "{commit}");
                let short = &commit[..commit.len().min(12)];
                assert_eq!(LONG_VERSION, format!("{VERSION} ({short})"));
            }
            None => assert_eq!(LONG_VERSION, VERSION),
        }
    }

    #[test]
    fn features_are_unique_kebab_case() {
        let mut seen = std::collections::BTreeSet::new();
        for f in FEATURES {
            assert!(seen.insert(f), "duplicate feature {f}");
            assert!(
                f.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "feature {f}"
            );
        }
    }
}
