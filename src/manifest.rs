//! Profile manifest (`rj.toml`) — the declarative front-end that collapses a
//! wall of Jenkins job parameters into a single named, version-controlled profile.
//!
//! A manifest lives next to the code it builds and is discovered by walking up
//! from the current directory (like `.git`). It maps a short profile name to a
//! job plus a bundle of build parameters, so a caller writes
//!
//! ```text
//! rj run nightly
//! ```
//!
//! instead of passing thirty `-p KEY=VALUE` flags. Profiles can `extends` one
//! another to share common settings, and CLI `-p` overrides always win.
//!
//! Everything in this module is pure (no I/O beyond `load`), which keeps the
//! inheritance and validation logic exhaustively unit-testable.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The parsed `rj.toml` file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// Settings applied when a more specific source (flag/env) doesn't provide them.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Fallback Jenkins base URL when `--url` / `JENKINS_URL` is not set.
    pub url: Option<String>,
}

/// One named profile. `job` and `params` may be omitted on a pure "mixin"
/// profile that only exists to be `extends`-ed by others.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Jenkins job path this profile builds (inherited from a parent if absent).
    pub job: Option<String>,
    /// Human description, shown by `rj profiles`.
    pub description: Option<String>,
    /// Name of another profile to inherit `job` and `params` from.
    pub extends: Option<String>,
    /// Build parameters. Values may be strings, numbers, or booleans in the
    /// TOML — they are all rendered to strings for Jenkins' form submission,
    /// so `PARALLELISM = 8` and `VERBOSE = true` don't need quoting.
    #[serde(default)]
    pub params: BTreeMap<String, toml::Value>,
}

/// A profile with its `extends` chain flattened and all values stringified —
/// ready to submit to Jenkins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    pub name: String,
    pub job: String,
    pub description: Option<String>,
    /// Sorted `(key, value)` pairs (BTreeMap iteration order) for deterministic
    /// output and reproducible builds.
    pub params: Vec<(String, String)>,
}

impl ResolvedProfile {
    /// Apply a CLI-supplied override, replacing any existing value for `key`.
    /// Overrides have the highest precedence, above every profile in the chain.
    pub fn set_param(&mut self, key: String, value: String) {
        match self.params.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => {
                self.params.push((key, value));
                self.params.sort_by(|a, b| a.0.cmp(&b.0));
            }
        }
    }
}

impl Manifest {
    /// Comma-separated list of defined profile names, for error messages.
    fn profile_names(&self) -> String {
        if self.profiles.is_empty() {
            "(none defined)".to_string()
        } else {
            self.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
        }
    }

    /// Flatten `name` and its `extends` ancestors into a single ready-to-run
    /// profile. Parameters from ancestors are applied first, so a child's value
    /// for the same key wins. Detects missing parents and `extends` cycles.
    pub fn resolve(&self, name: &str) -> Result<ResolvedProfile> {
        // Walk the extends chain leaf → root, recording each link.
        let mut chain: Vec<&Profile> = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        let mut current = name;

        loop {
            if seen.contains(&current) {
                bail!(
                    "circular 'extends' detected in manifest involving profile '{current}' \
                     (chain: {})",
                    seen.join(" → ")
                );
            }
            let profile = self.profiles.get(current).ok_or_else(|| {
                if seen.is_empty() {
                    anyhow!(
                        "profile '{current}' not found in manifest.\nAvailable profiles: {}",
                        self.profile_names()
                    )
                } else {
                    anyhow!(
                        "profile '{}' extends '{current}', which is not defined in the manifest",
                        seen.last().unwrap()
                    )
                }
            })?;
            seen.push(current);
            chain.push(profile);
            match &profile.extends {
                Some(parent) => current = parent,
                None => break,
            }
        }

        // Apply root → leaf so the most specific (leaf) profile wins.
        let mut params: BTreeMap<String, String> = BTreeMap::new();
        let mut job: Option<String> = None;
        let mut description: Option<String> = None;

        for profile in chain.iter().rev() {
            if profile.job.is_some() {
                job = profile.job.clone();
            }
            if profile.description.is_some() {
                description = profile.description.clone();
            }
            for (key, value) in &profile.params {
                params.insert(key.clone(), value_to_string(key, value)?);
            }
        }

        let job = job.ok_or_else(|| {
            anyhow!(
                "profile '{name}' does not define a 'job' (and neither do its parents) — \
                 add `job = \"folder/my-job\"` to the profile or one it extends"
            )
        })?;

        Ok(ResolvedProfile {
            name: name.to_string(),
            job,
            description,
            params: params.into_iter().collect(),
        })
    }
}

/// Render a scalar TOML value to the string Jenkins expects. Arrays and tables
/// are rejected: a Jenkins build parameter is always a single scalar.
fn value_to_string(key: &str, value: &toml::Value) -> Result<String> {
    Ok(match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        other => bail!(
            "parameter '{key}' has unsupported type '{}' — Jenkins build parameters \
             must be a single scalar (string, number, or boolean)",
            other.type_str()
        ),
    })
}

// ── Discovery & loading ────────────────────────────────────────────────────────

/// Search `start` and each parent directory for an `rj.toml`, returning the
/// first one found. Mirrors how git locates its repository root.
pub fn discover(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join("rj.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// Read and parse a manifest from disk.
pub fn load(path: &Path) -> Result<Manifest> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading manifest '{}'", path.display()))?;
    parse(&text).with_context(|| format!("parsing manifest '{}'", path.display()))
}

/// Parse a manifest from a string (split out so tests need no filesystem).
pub fn parse(text: &str) -> Result<Manifest> {
    toml::from_str(text).map_err(|e| anyhow!("{e}"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [defaults]
        url = "https://jenkins.example.com"

        [profiles.base]
        job = "platform/integration-tests"
        description = "shared settings"
        [profiles.base.params]
        REGION = "us-east-1"
        RETRIES = 3
        VERBOSE = false

        [profiles.nightly]
        extends = "base"
        description = "full nightly suite"
        [profiles.nightly.params]
        SUITE = "full"
        PARALLELISM = 8

        [profiles.smoke]
        extends = "base"
        [profiles.smoke.params]
        SUITE = "smoke"
        RETRIES = 1
    "#;

    fn manifest() -> Manifest {
        parse(SAMPLE).unwrap()
    }

    #[test]
    fn parses_defaults_and_profiles() {
        let m = manifest();
        assert_eq!(m.defaults.url.as_deref(), Some("https://jenkins.example.com"));
        assert_eq!(m.profiles.len(), 3);
    }

    #[test]
    fn resolve_flattens_extends_chain() {
        let r = manifest().resolve("nightly").unwrap();
        assert_eq!(r.job, "platform/integration-tests"); // inherited from base
        assert_eq!(r.description.as_deref(), Some("full nightly suite")); // leaf wins
    }

    #[test]
    fn resolve_stringifies_scalar_types() {
        let r = manifest().resolve("nightly").unwrap();
        let map: BTreeMap<_, _> = r.params.into_iter().collect();
        assert_eq!(map["REGION"], "us-east-1"); // inherited string
        assert_eq!(map["RETRIES"], "3"); // integer → string
        assert_eq!(map["VERBOSE"], "false"); // bool → string
        assert_eq!(map["PARALLELISM"], "8"); // own integer
        assert_eq!(map["SUITE"], "full");
    }

    #[test]
    fn child_param_overrides_parent() {
        // smoke sets RETRIES = 1, base has RETRIES = 3.
        let r = manifest().resolve("smoke").unwrap();
        let map: BTreeMap<_, _> = r.params.into_iter().collect();
        assert_eq!(map["RETRIES"], "1");
        assert_eq!(map["REGION"], "us-east-1"); // still inherited
    }

    #[test]
    fn resolved_params_are_sorted_for_determinism() {
        let r = manifest().resolve("nightly").unwrap();
        let keys: Vec<&str> = r.params.iter().map(|(k, _)| k.as_str()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn set_param_override_replaces_existing() {
        let mut r = manifest().resolve("nightly").unwrap();
        r.set_param("SUITE".into(), "regression".into());
        let map: BTreeMap<_, _> = r.params.into_iter().collect();
        assert_eq!(map["SUITE"], "regression");
    }

    #[test]
    fn set_param_override_adds_new_key_and_keeps_sorted() {
        let mut r = manifest().resolve("nightly").unwrap();
        r.set_param("AAA_NEW".into(), "x".into());
        let keys: Vec<&str> = r.params.iter().map(|(k, _)| k.as_str()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "params must stay sorted after an insert");
    }

    #[test]
    fn resolve_unknown_profile_errors_with_available_list() {
        let err = manifest().resolve("does-not-exist").unwrap_err().to_string();
        assert!(err.contains("not found"));
        assert!(err.contains("nightly"), "should list available profiles: {err}");
    }

    #[test]
    fn resolve_missing_parent_names_the_offender() {
        let m = parse(
            r#"
            [profiles.child]
            job = "j"
            extends = "ghost"
        "#,
        )
        .unwrap();
        let err = m.resolve("child").unwrap_err().to_string();
        assert!(err.contains("ghost"));
        assert!(err.contains("child"));
    }

    #[test]
    fn resolve_detects_extends_cycle() {
        let m = parse(
            r#"
            [profiles.a]
            job = "j"
            extends = "b"
            [profiles.b]
            extends = "a"
        "#,
        )
        .unwrap();
        let err = m.resolve("a").unwrap_err().to_string();
        assert!(err.contains("circular"), "got: {err}");
    }

    #[test]
    fn resolve_errors_when_no_job_anywhere_in_chain() {
        let m = parse(
            r#"
            [profiles.p]
            [profiles.p.params]
            X = "1"
        "#,
        )
        .unwrap();
        let err = m.resolve("p").unwrap_err().to_string();
        assert!(err.contains("does not define a 'job'"), "got: {err}");
    }

    #[test]
    fn array_valued_param_is_rejected() {
        let m = parse(
            r#"
            [profiles.p]
            job = "j"
            [profiles.p.params]
            LIST = ["a", "b"]
        "#,
        )
        .unwrap();
        let err = m.resolve("p").unwrap_err().to_string();
        assert!(err.contains("unsupported type"), "got: {err}");
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        // deny_unknown_fields catches typos like `profile` vs `profiles`.
        let err = parse(
            r#"
            [profile.oops]
            job = "j"
        "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown field") || err.contains("profile"), "got: {err}");
    }

    #[test]
    fn multi_level_extends_chain_resolves() {
        let m = parse(
            r#"
            [profiles.grandparent]
            job = "g"
            [profiles.grandparent.params]
            A = "1"
            [profiles.parent]
            extends = "grandparent"
            [profiles.parent.params]
            B = "2"
            [profiles.child]
            extends = "parent"
            [profiles.child.params]
            C = "3"
        "#,
        )
        .unwrap();
        let r = m.resolve("child").unwrap();
        assert_eq!(r.job, "g");
        let map: BTreeMap<_, _> = r.params.into_iter().collect();
        assert_eq!(map["A"], "1");
        assert_eq!(map["B"], "2");
        assert_eq!(map["C"], "3");
    }

    // ── discovery ─────────────────────────────────────────────────────────────

    #[test]
    fn discover_finds_manifest_in_parent_directory() {
        let tmp = std::env::temp_dir().join(format!("rj_manifest_disc_{}", std::process::id()));
        let nested = tmp.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(tmp.join("rj.toml"), "[profiles.x]\njob=\"j\"\n").unwrap();

        let found = discover(&nested).expect("should find rj.toml walking upward");
        assert_eq!(found, tmp.join("rj.toml"));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn discover_returns_none_when_absent() {
        let tmp = std::env::temp_dir().join(format!("rj_manifest_none_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(discover(&tmp).is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
