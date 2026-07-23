//! `rj profiles` — list the profiles defined in the manifest.
//!
//! Read-only and offline: it never contacts Jenkins, so it's a safe way to see
//! what's available (and, with `--verbose`, exactly what each profile resolves
//! to after `extends` inheritance) before running anything.

use crate::cli::ProfilesArgs;
use crate::manifest::Manifest;
use anyhow::Result;
use colored::Colorize;

pub fn run(manifest: &Manifest, args: &ProfilesArgs) -> Result<()> {
    if manifest.profiles.is_empty() {
        println!("{}", "No profiles defined in the manifest.".dimmed());
        return Ok(());
    }

    for name in manifest.profiles.keys() {
        // Resolve so we report the effective job/params, including inheritance.
        match manifest.resolve(name) {
            Ok(r) => {
                let desc = r
                    .description
                    .as_deref()
                    .map(|d| format!("  {}", format!("— {d}").dimmed()))
                    .unwrap_or_default();
                println!("{}{}", name.cyan().bold(), desc);
                println!("  job: {}", r.job.dimmed());
                if args.verbose {
                    for (k, v) in &r.params {
                        println!("    {:<24} = {}", k, v.yellow());
                    }
                } else {
                    println!("  {}", format!("{} parameter(s)", r.params.len()).dimmed());
                }
            }
            // A profile that can't resolve (bad extends, no job) is still worth
            // surfacing by name rather than aborting the whole listing.
            Err(e) => {
                println!("{}", name.cyan().bold());
                println!("  {} {e:#}", "unresolved —".red());
            }
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse;

    fn args(verbose: bool) -> ProfilesArgs {
        ProfilesArgs { verbose }
    }

    #[test]
    fn lists_without_error_when_profiles_present() {
        let m = parse(
            r#"
            [profiles.a]
            job = "j"
            description = "the a profile"
            [profiles.a.params]
            X = "1"
        "#,
        )
        .unwrap();
        run(&m, &args(false)).unwrap();
        run(&m, &args(true)).unwrap();
    }

    #[test]
    fn empty_manifest_lists_without_error() {
        let m = Manifest::default();
        run(&m, &args(false)).unwrap();
    }

    #[test]
    fn unresolvable_profile_does_not_abort_listing() {
        // `broken` extends a missing parent; listing should still succeed.
        let m = parse(
            r#"
            [profiles.ok]
            job = "j"
            [profiles.broken]
            extends = "ghost"
        "#,
        )
        .unwrap();
        run(&m, &args(false)).unwrap();
    }
}
