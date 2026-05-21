//! Read session cookies from the Firefox cookie database.
//!
//! After logging into Jenkins via SSO in Firefox, this module extracts the
//! relevant cookies so `rj` can authenticate API requests without Basic Auth.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

// ── Public entry point ────────────────────────────────────────────────────────

/// Return a `Cookie: …` header value built from every non-expired session
/// cookie Firefox holds for the Jenkins hostname.
pub fn firefox_cookies(jenkins_url: &str) -> Result<String> {
    let hostname = extract_hostname(jenkins_url)?;
    let db_path = find_cookie_db()?;
    read_cookies(&db_path, &hostname)
}

// ── Hostname extraction ───────────────────────────────────────────────────────

/// Strip scheme, path, and port from a URL, leaving just the hostname.
/// "https://ci.example.com:8080/controller" → "ci.example.com"
pub fn extract_hostname(url: &str) -> Result<String> {
    let without_scheme = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");

    let host_and_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    let hostname = host_and_port.split(':').next().unwrap_or(host_and_port);

    if hostname.is_empty() {
        anyhow::bail!("could not extract hostname from URL: {url}");
    }
    Ok(hostname.to_string())
}

// ── Firefox profile discovery ─────────────────────────────────────────────────

fn firefox_base_dir() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").context("APPDATA env var not set")?;
        Ok(PathBuf::from(appdata).join("Mozilla").join("Firefox"))
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").context("HOME env var not set")?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Firefox"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let home = std::env::var("HOME").context("HOME env var not set")?;
        Ok(PathBuf::from(home).join(".mozilla").join("firefox"))
    }
}

fn find_cookie_db() -> Result<PathBuf> {
    let base = firefox_base_dir()?;
    let ini_path = base.join("profiles.ini");

    if !ini_path.exists() {
        anyhow::bail!(
            "Firefox profiles.ini not found at '{}'. Is Firefox installed?",
            ini_path.display()
        );
    }

    let ini = std::fs::read_to_string(&ini_path)
        .with_context(|| format!("reading {}", ini_path.display()))?;

    let profile_path = parse_default_profile(&ini)
        .ok_or_else(|| anyhow::anyhow!("no default Firefox profile found in profiles.ini"))?;

    let absolute = if Path::new(&profile_path).is_absolute() {
        PathBuf::from(&profile_path)
    } else {
        base.join(&profile_path)
    };

    let db = absolute.join("cookies.sqlite");
    if !db.exists() {
        anyhow::bail!(
            "Firefox cookie database not found at '{}'.\n\
             Make sure you have logged into Jenkins in Firefox at least once.",
            db.display()
        );
    }

    Ok(db)
}

/// Parse `profiles.ini` and return the path of the default profile.
/// Prefers the `[Install*]` section's `Default` key (most reliable),
/// falling back to any `[Profile*]` section with `Default=1`.
pub fn parse_default_profile(ini: &str) -> Option<String> {
    let mut install_default: Option<String> = None;
    let mut profile_default: Option<String> = None;

    let mut in_install = false;
    let mut in_profile = false;
    let mut cur_path: Option<String> = None;
    let mut cur_is_default = false;

    for line in ini.lines() {
        let line = line.trim();

        if line.starts_with('[') && line.ends_with(']') {
            // Flush the completed Profile section
            if in_profile && cur_is_default {
                profile_default = cur_path.take();
            }
            let section = &line[1..line.len() - 1];
            in_install = section.starts_with("Install");
            in_profile = section.starts_with("Profile");
            cur_path = None;
            cur_is_default = false;
        } else if let Some((key, value)) = line.split_once('=') {
            let (k, v) = (key.trim(), value.trim());
            if in_install && k == "Default" {
                install_default = Some(v.to_string());
            } else if in_profile {
                match k {
                    "Path"    => cur_path = Some(v.to_string()),
                    "Default" if v == "1" => cur_is_default = true,
                    _ => {}
                }
            }
        }
    }

    // Flush last section
    if in_profile && cur_is_default {
        profile_default = cur_path;
    }

    install_default.or(profile_default)
}

// ── Cookie database query ─────────────────────────────────────────────────────

fn read_cookies(db_path: &Path, hostname: &str) -> Result<String> {
    // Copy to a temp file so we can read it safely while Firefox is running.
    let tmp = std::env::temp_dir().join("rj_firefox_cookies.sqlite");
    std::fs::copy(db_path, &tmp)
        .with_context(|| format!("copying Firefox cookies from '{}'", db_path.display()))?;

    let conn = Connection::open_with_flags(&tmp, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening Firefox cookie database")?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Firefox stores cookies with host = "example.com" or host = ".example.com"
    let mut stmt = conn
        .prepare(
            "SELECT name, value FROM moz_cookies
              WHERE (host = ?1 OR host = ?2)
                AND expiry > ?3
              ORDER BY name",
        )
        .context("preparing cookie query")?;

    let cookies: Vec<String> = stmt
        .query_map(
            rusqlite::params![hostname, format!(".{hostname}"), now],
            |row| {
                let name: String = row.get(0)?;
                let value: String = row.get(1)?;
                Ok(format!("{name}={value}"))
            },
        )
        .context("querying cookies")?
        .filter_map(|r| r.ok())
        .collect();

    std::fs::remove_file(&tmp).ok();

    if cookies.is_empty() {
        anyhow::bail!(
            "no valid session cookies found for '{}' in Firefox.\n\
             Log into Jenkins in Firefox first, then re-run this command.",
            hostname
        );
    }

    Ok(cookies.join("; "))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_hostname ──────────────────────────────────────────────────────

    #[test]
    fn extract_hostname_strips_https_and_port() {
        assert_eq!(
            extract_hostname("https://jenkins.example.com:8080").unwrap(),
            "jenkins.example.com"
        );
    }

    #[test]
    fn extract_hostname_strips_path_prefix() {
        assert_eq!(
            extract_hostname("https://ci.example.com/controller13").unwrap(),
            "ci.example.com"
        );
    }

    #[test]
    fn extract_hostname_handles_http_and_no_port() {
        assert_eq!(
            extract_hostname("http://jenkins.local").unwrap(),
            "jenkins.local"
        );
    }

    #[test]
    fn extract_hostname_handles_deep_path_and_port() {
        assert_eq!(
            extract_hostname("https://ci.example.com:8443/a/b/c").unwrap(),
            "ci.example.com"
        );
    }

    // ── parse_default_profile ─────────────────────────────────────────────────

    #[test]
    fn parse_default_profile_prefers_install_section() {
        let ini = "[Install308046B0AF4A39CB]\n\
                   Default=Profiles/abc.default-release\n\
                   Locked=1\n\
                   \n\
                   [Profile0]\n\
                   Name=default-release\n\
                   IsRelative=1\n\
                   Path=Profiles/abc.default-release\n\
                   Default=1\n";
        assert_eq!(
            parse_default_profile(ini),
            Some("Profiles/abc.default-release".to_string())
        );
    }

    #[test]
    fn parse_default_profile_falls_back_to_profile_default_1() {
        let ini = "[Profile0]\n\
                   Name=default-release\n\
                   IsRelative=1\n\
                   Path=Profiles/xyz.default\n\
                   Default=1\n";
        assert_eq!(
            parse_default_profile(ini),
            Some("Profiles/xyz.default".to_string())
        );
    }

    #[test]
    fn parse_default_profile_picks_correct_profile_when_multiple_exist() {
        let ini = "[Profile0]\n\
                   Name=work\n\
                   IsRelative=1\n\
                   Path=Profiles/aaa.work\n\
                   \n\
                   [Profile1]\n\
                   Name=default-release\n\
                   IsRelative=1\n\
                   Path=Profiles/bbb.default-release\n\
                   Default=1\n";
        assert_eq!(
            parse_default_profile(ini),
            Some("Profiles/bbb.default-release".to_string())
        );
    }

    #[test]
    fn parse_default_profile_returns_none_when_no_default_marked() {
        let ini = "[Profile0]\n\
                   Name=default-release\n\
                   IsRelative=1\n\
                   Path=Profiles/abc.default\n";
        assert!(parse_default_profile(ini).is_none());
    }
}
