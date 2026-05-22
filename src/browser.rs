//! Read session cookies from Firefox or Chrome.
//!
//! After logging into Jenkins via SSO in a browser, this module extracts the
//! relevant cookies so `rj` can authenticate API requests without Basic Auth.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// ── Public entry points ───────────────────────────────────────────────────────

/// Return a `Cookie: …` header value from the Firefox cookie database.
pub fn firefox_cookies(jenkins_url: &str) -> Result<String> {
    let hostname = extract_hostname(jenkins_url)?;
    let db = find_firefox_cookie_db()?;
    query_firefox_cookies(&db, &hostname)
}

/// Return a `Cookie: …` header value from the Chrome cookie database.
/// `profile` is the Chrome profile folder name (e.g. `"Default"`, `"Profile 1"`).
pub fn chrome_cookies(jenkins_url: &str, profile: &str) -> Result<String> {
    let hostname = extract_hostname(jenkins_url)?;
    let db = find_chrome_cookie_db(profile)?;
    let key = chrome_master_key(profile)?;
    query_chrome_cookies(&db, &hostname, &key)
}

/// Print the names of every cookie found for the Jenkins hostname — no values.
/// Useful for diagnosing auth issues without exposing secrets.
pub fn list_cookie_names(jenkins_url: &str, browser: &str, profile: &str) -> Result<()> {
    let hostname = extract_hostname(jenkins_url)?;
    println!("Looking for cookies matching host: {hostname}");

    let names: Vec<String> = match browser {
        "chrome" => {
            let db = find_chrome_cookie_db(profile)?;
            let key = chrome_master_key(profile)?;
            cookie_names_chrome(&db, &hostname, &key)?
        }
        "firefox" => {
            let db = find_firefox_cookie_db()?;
            cookie_names_firefox(&db, &hostname)?
        }
        other => anyhow::bail!("unknown browser '{other}' — use 'chrome' or 'firefox'"),
    };

    if names.is_empty() {
        println!("No cookies found.");
        println!();
        println!("Troubleshooting:");
        println!("  • Make sure you are logged into Jenkins in {browser}");
        if browser == "chrome" {
            println!("  • Check your active Chrome profile: open chrome://version and look for");
            println!("    'Profile Path'. Pass the folder name with --chrome-profile \"<name>\"");
            println!("    Common names: \"Default\", \"Profile 1\", \"Profile 2\"");
        }
        println!("  • The cookie may have expired — log in again and retry");
    } else {
        let auth: Vec<&str> = names.iter()
            .filter(|n| n.starts_with("JSESSIONID"))
            .map(String::as_str)
            .collect();

        println!("Found {} cookie(s):", names.len());
        for name in &names {
            if name.starts_with("JSESSIONID") {
                println!("  {name}  ← auth");
            } else {
                println!("  {name}  (preference, ignored)");
            }
        }
        println!();
        if auth.is_empty() {
            println!("No JSESSIONID cookies found — you may not be logged in.");
        } else {
            println!("rj will use: {}", auth.join(", "));
            println!("Run with --from-{browser} to authenticate.");
        }
    }

    Ok(())
}

// ── Hostname extraction ───────────────────────────────────────────────────────

/// Strip scheme, path, and port: "https://ci.example.com:8443/ctrl" → "ci.example.com"
pub fn extract_hostname(url: &str) -> Result<String> {
    let without_scheme = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    let hostname = host_port.split(':').next().unwrap_or(host_port);
    if hostname.is_empty() {
        anyhow::bail!("could not extract hostname from URL: {url}");
    }
    Ok(hostname.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Firefox
// ─────────────────────────────────────────────────────────────────────────────

fn firefox_base_dir() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").context("APPDATA env var not set")?;
        Ok(PathBuf::from(appdata).join("Mozilla").join("Firefox"))
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Firefox"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(PathBuf::from(home).join(".mozilla").join("firefox"))
    }
}

fn find_firefox_cookie_db() -> Result<PathBuf> {
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
    let rel = parse_default_profile(&ini)
        .ok_or_else(|| anyhow::anyhow!("no default Firefox profile found in profiles.ini"))?;
    let profile_dir = if Path::new(&rel).is_absolute() {
        PathBuf::from(&rel)
    } else {
        base.join(&rel)
    };
    let db = profile_dir.join("cookies.sqlite");
    if !db.exists() {
        anyhow::bail!(
            "Firefox cookie database not found at '{}'. Log into Jenkins in Firefox first.",
            db.display()
        );
    }
    Ok(db)
}

/// Parse `profiles.ini`: prefer `[Install*].Default`, fall back to `[Profile*] Default=1`.
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
            if in_profile && cur_is_default {
                profile_default = cur_path.take();
            }
            let section = &line[1..line.len() - 1];
            in_install = section.starts_with("Install");
            in_profile = section.starts_with("Profile");
            cur_path = None;
            cur_is_default = false;
        } else if let Some((k, v)) = line.split_once('=') {
            let (k, v) = (k.trim(), v.trim());
            if in_install && k == "Default" {
                install_default = Some(v.to_string());
            } else if in_profile {
                match k {
                    "Path" => cur_path = Some(v.to_string()),
                    "Default" if v == "1" => cur_is_default = true,
                    _ => {}
                }
            }
        }
    }
    if in_profile && cur_is_default {
        profile_default = cur_path;
    }
    install_default.or(profile_default)
}

fn query_firefox_cookies(db_path: &Path, hostname: &str) -> Result<String> {
    let tmp = try_copy_db(db_path)?;
    let conn = Connection::open_with_flags(&tmp, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening Firefox cookie database")?;

    let now_unix = unix_now_secs() as i64;
    let mut stmt = conn
        .prepare(
            // Only JSESSIONID* cookies are used for Jenkins auth.
            // Other cookies (timestamper, screenResolution, etc.) are preferences only.
            "SELECT name, value FROM moz_cookies
              WHERE (host = ?1 OR host = ?2)
                AND expiry > ?3
                AND name LIKE 'JSESSIONID%'
              ORDER BY name",
        )
        .context("preparing Firefox cookie query")?;

    let cookies: Vec<String> = stmt
        .query_map(
            rusqlite::params![hostname, format!(".{hostname}"), now_unix],
            |row| Ok(format!("{}={}", row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .context("querying Firefox cookies")?
        .filter_map(|r| r.ok())
        .collect();

    std::fs::remove_file(&tmp).ok();
    require_nonempty(cookies, hostname)
}

// ─────────────────────────────────────────────────────────────────────────────
// Chrome
// ─────────────────────────────────────────────────────────────────────────────

fn chrome_base_dir() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let local = std::env::var("LOCALAPPDATA").context("LOCALAPPDATA env var not set")?;
        Ok(PathBuf::from(local).join("Google").join("Chrome").join("User Data"))
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Google")
            .join("Chrome"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(PathBuf::from(home).join(".config").join("google-chrome"))
    }
}

fn find_chrome_cookie_db(profile: &str) -> Result<PathBuf> {
    let base = chrome_base_dir()?;
    // Chrome 96+ moved the file to Network/Cookies; older versions kept it in <profile>/Cookies
    let candidates = [
        base.join(profile).join("Network").join("Cookies"),
        base.join(profile).join("Cookies"),
    ];
    candidates
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Chrome cookie database not found for profile '{profile}'.\n\
                 Check your active profile name at chrome://version (look for 'Profile Path').\n\
                 Pass it with --chrome-profile \"Profile 1\" (or whichever name matches)."
            )
        })
}

/// Decrypt the Chrome AES master key.
/// On Windows: read from `Local State`, base64-decode, DPAPI-decrypt.
/// On macOS: read from Keychain (not yet implemented — see note).
fn chrome_master_key(profile: &str) -> Result<Vec<u8>> {
    // profile is unused on Windows/macOS (key is global, not per-profile)
    let _ = profile;
    #[cfg(target_os = "windows")]
    {
        let base = chrome_base_dir()?;
        let state_path = base.join("Local State");
        let state_json = std::fs::read_to_string(&state_path)
            .with_context(|| format!("reading Chrome Local State at '{}'", state_path.display()))?;

        // Parse: {"os_crypt":{"encrypted_key":"<base64>"}}
        let json: serde_json::Value =
            serde_json::from_str(&state_json).context("parsing Chrome Local State JSON")?;
        let b64 = json
            .get("os_crypt")
            .and_then(|v| v.get("encrypted_key"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("os_crypt.encrypted_key not found in Chrome Local State"))?;

        use base64::Engine;
        let mut encrypted = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .context("base64-decoding Chrome master key")?;

        // Strip the "DPAPI" prefix Chrome prepends before encrypting
        const PREFIX: &[u8] = b"DPAPI";
        if encrypted.starts_with(PREFIX) {
            encrypted.drain(..PREFIX.len());
        }

        dpapi::decrypt(&encrypted)
    }

    #[cfg(target_os = "macos")]
    {
        // macOS Chrome stores the encryption password in the Keychain.
        // The `security` CLI reads it without needing a native Keychain API binding.
        // Note: the first run may show a Keychain permission prompt in the terminal.
        let output = std::process::Command::new("security")
            .args([
                "find-generic-password",
                "-s", "Chrome Safe Storage",
                "-a", "Chrome",
                "-w", // output only the password, no metadata
            ])
            .output()
            .context("running 'security' to read Chrome Keychain entry")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Could not read Chrome Safe Storage password from Keychain: {stderr}\n\
                 Make sure Chrome has been launched at least once on this Mac."
            );
        }

        let password = String::from_utf8(output.stdout)
            .context("Keychain password is not valid UTF-8")?
            .trim()
            .to_string();

        // Derive 48 bytes via PBKDF2-HMAC-SHA1:
        //   [0..16]  → 16-byte CBC key  (pre-Chrome-127 AES-128-CBC scheme)
        //   [0..32]  → 32-byte GCM key  (Chrome 127+ AES-256-GCM scheme)
        // chrome_decrypt tries CBC first, then GCM as fallback.
        let mut key = vec![0u8; 48];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password.as_bytes(), b"saltysalt", 1003, &mut key);
        Ok(key)
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // Linux Chrome uses a hardcoded key or system keyring.
        // Return empty to signal plaintext-only fallback in query_chrome_cookies.
        Ok(Vec::new())
    }
}

fn query_chrome_cookies(db_path: &Path, hostname: &str, master_key: &[u8]) -> Result<String> {
    let conn = open_chrome_db(db_path)?;

    // Chrome expiry is microseconds since 1601-01-01; session cookies are 0.
    // Convert Unix now → Chrome epoch: add the 369-year offset in µs.
    let chrome_now = unix_now_secs() * 1_000_000 + 11_644_473_600_000_000u64;

    let mut stmt = conn
        .prepare(
            // Only JSESSIONID* cookies are used for Jenkins auth.
            "SELECT name, value, encrypted_value FROM cookies
              WHERE (host_key = ?1 OR host_key = ?2)
                AND (expires_utc = 0 OR expires_utc > ?3)
                AND name LIKE 'JSESSIONID%'
              ORDER BY name",
        )
        .context("preparing Chrome cookie query")?;

    let cookies: Vec<String> = stmt
        .query_map(
            rusqlite::params![hostname, format!(".{hostname}"), chrome_now as i64],
            |row| {
                let name: String = row.get(0)?;
                let value: String = row.get(1)?;
                let enc: Vec<u8> = row.get(2)?;
                Ok((name, value, enc))
            },
        )
        .context("querying Chrome cookies")?
        .filter_map(|r| r.ok())
        .filter_map(|(name, plaintext, enc)| {
            if !plaintext.is_empty() {
                return Some(format!("{name}={plaintext}"));
            }
            if enc.is_empty() {
                eprintln!("  [debug] {name}: both value and encrypted_value are empty — skipping");
                return None;
            }
            if master_key.is_empty() {
                eprintln!("  [debug] {name}: no master key available — skipping");
                return None;
            }
            match chrome_decrypt(&enc, master_key) {
                Ok(v) => Some(format!("{name}={v}")),
                Err(e) => {
                    eprintln!("  [debug] {name}: decryption failed — {e}");
                    None
                }
            }
        })
        .collect();

    require_nonempty(cookies, hostname)
}

/// Decrypt a single Chrome cookie value — dispatches to the right algorithm per OS.
fn cookie_names_firefox(db_path: &Path, hostname: &str) -> Result<Vec<String>> {
    let tmp = try_copy_db(db_path)?;
    let conn = Connection::open_with_flags(&tmp, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let now = unix_now_secs() as i64;
    let mut stmt = conn.prepare(
        "SELECT name FROM moz_cookies
          WHERE (host = ?1 OR host = ?2) AND expiry > ?3
          ORDER BY name",
    )?;
    let names = stmt
        .query_map(rusqlite::params![hostname, format!(".{hostname}"), now], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    std::fs::remove_file(&tmp).ok();
    Ok(names)
}

fn cookie_names_chrome(db_path: &Path, hostname: &str, master_key: &[u8]) -> Result<Vec<String>> {
    let conn = open_chrome_db(db_path)?;
    let chrome_now = unix_now_secs() * 1_000_000 + 11_644_473_600_000_000u64;
    let mut stmt = conn.prepare(
        "SELECT name FROM cookies
          WHERE (host_key = ?1 OR host_key = ?2)
            AND (expires_utc = 0 OR expires_utc > ?3)
          ORDER BY name",
    )?;
    let names: Vec<String> = stmt
        .query_map(
            rusqlite::params![hostname, format!(".{hostname}"), chrome_now as i64],
            |r| r.get(0),
        )?
        .filter_map(|r| r.ok())
        // Only list names we can actually use (have a value, encrypted or plain)
        .collect();
    let _ = master_key; // names-only query, no decryption needed
    Ok(names)
}

fn chrome_decrypt(encrypted: &[u8], key: &[u8]) -> Result<String> {
    #[cfg(target_os = "windows")]
    return chrome_decrypt_gcm(encrypted, key);

    #[cfg(target_os = "macos")]
    return chrome_decrypt_macos(encrypted, key);

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (encrypted, key);
        anyhow::bail!("Chrome cookie decryption is not supported on this platform")
    }
}

/// macOS: try every plausible decryption combination and print full hex dumps.
#[cfg(target_os = "macos")]
fn chrome_decrypt_macos(encrypted: &[u8], key: &[u8]) -> Result<String> {
    // key[0..48] from PBKDF2: [0..16]=CBC key, [0..32]=GCM key

    eprintln!(
        "[debug] encrypted ({} bytes): {:02x?}",
        encrypted.len(), encrypted
    );
    eprintln!("[debug] keychain-cbc-key (16B): {:02x?}", &key[..16]);

    // Helper: try decrypted bytes at multiple offsets, return first valid UTF-8
    fn try_offsets(label: &str, raw: &[u8]) -> Option<String> {
        eprintln!("[debug] {label} raw ({} bytes): {:02x?}", raw.len(), raw);
        for offset in [0usize, 16, 32] {
            if raw.len() > offset {
                match std::str::from_utf8(&raw[offset..]) {
                    Ok(s) => {
                        let s = s.trim_end_matches('\0').to_string();
                        if !s.is_empty() {
                            eprintln!("[debug] {label} offset={offset} → valid UTF-8: {s:?}");
                            return Some(s);
                        }
                        eprintln!("[debug] {label} offset={offset} → valid UTF-8 but empty after trim");
                    }
                    Err(e) => eprintln!(
                        "[debug] {label} offset={offset} → not UTF-8 at byte {}: {:02x?}",
                        e.valid_up_to(), &raw[offset..]
                    ),
                }
            }
        }
        None
    }

    // ── S1: Keychain key + fixed IV [0x20×16] ────────────────────────────────
    match cbc_decrypt_raw_iv(encrypted, &key[..16], &[0x20u8; 16]) {
        Ok(raw) => { if let Some(v) = try_offsets("keychain-cbc-fixed-iv", &raw) { return Ok(v); } }
        Err(e) => eprintln!("[debug] keychain-cbc-fixed-iv failed: {e}"),
    }

    // ── S2: Keychain key + embedded IV (bytes 3..19 are the IV) ─────────────
    // Some Chrome builds store a per-cookie IV inside the ciphertext:
    // [v10 (3B)] [IV (16B)] [ciphertext]
    if encrypted.len() >= 3 + 16 + 16 {
        let embedded_iv = &encrypted[3..19];
        let ciphertext_with_embedded_iv = [b"v10", encrypted[19..].as_ref()].concat();
        eprintln!("[debug] embedded-iv: {:02x?}", embedded_iv);
        match cbc_decrypt_raw_iv(&ciphertext_with_embedded_iv, &key[..16], embedded_iv) {
            Ok(raw) => { if let Some(v) = try_offsets("keychain-cbc-embedded-iv", &raw) { return Ok(v); } }
            Err(e) => eprintln!("[debug] keychain-cbc-embedded-iv failed: {e}"),
        }
    }

    // ── S3: Keychain key + AES-256-GCM ───────────────────────────────────────
    eprintln!("[debug] keychain-gcm-key (32B): {:02x?}", &key[..32]);
    match chrome_decrypt_gcm(encrypted, &key[..32]) {
        Ok(v) => { eprintln!("[debug] keychain-gcm → OK"); return Ok(v); }
        Err(e) => eprintln!("[debug] keychain-gcm failed: {e}"),
    }

    // ── S4: peanuts + fixed IV ────────────────────────────────────────────────
    let mut peanuts_key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(b"peanuts", b"saltysalt", 1, &mut peanuts_key);
    eprintln!("[debug] peanuts-key: {:02x?}", peanuts_key);
    match cbc_decrypt_raw_iv(encrypted, &peanuts_key, &[0x20u8; 16]) {
        Ok(raw) => { if let Some(v) = try_offsets("peanuts-cbc-fixed-iv", &raw) { return Ok(v); } }
        Err(e) => eprintln!("[debug] peanuts-cbc-fixed-iv failed: {e}"),
    }

    // ── S5: peanuts + zero IV ─────────────────────────────────────────────────
    match cbc_decrypt_raw_iv(encrypted, &peanuts_key, &[0x00u8; 16]) {
        Ok(raw) => { if let Some(v) = try_offsets("peanuts-cbc-zero-iv", &raw) { return Ok(v); } }
        Err(e) => eprintln!("[debug] peanuts-cbc-zero-iv failed: {e}"),
    }

    anyhow::bail!("all macOS decryption strategies failed — see [debug] lines above")
}

/// Raw AES-128-CBC with an explicit IV (no UTF-8 check, returns raw plaintext bytes).
#[cfg(target_os = "macos")]
fn cbc_decrypt_raw_iv(encrypted: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>> {
    use aes::Aes128;
    use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    type Aes128CbcDec = cbc::Decryptor<Aes128>;

    if encrypted.len() < 3 { anyhow::bail!("too short"); }
    let key16: &[u8; 16] = key.try_into()
        .map_err(|_| anyhow::anyhow!("key must be 16 bytes (got {})", key.len()))?;
    let iv16: &[u8; 16] = iv.try_into()
        .map_err(|_| anyhow::anyhow!("IV must be 16 bytes"))?;
    let mut buf = encrypted[3..].to_vec();
    let out = Aes128CbcDec::new(key16.into(), iv16.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| anyhow::anyhow!("PKCS7 failed"))?;
    Ok(out.to_vec())
}


/// AES-256-GCM. Format: "v10"|"v20" (3 B) + nonce (12 B) + ciphertext+tag.
/// Used on Windows always; used on macOS as fallback for Chrome 127+ which
/// switched from AES-128-CBC to AES-256-GCM while keeping the "v10" prefix.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn chrome_decrypt_gcm(encrypted: &[u8], key: &[u8]) -> Result<String> {
    use aes_gcm::{
        aead::{Aead, KeyInit},
        Aes256Gcm, Nonce,
    };

    const PREFIX: usize = 3;
    const NONCE:  usize = 12;

    if encrypted.len() < PREFIX + NONCE {
        anyhow::bail!("encrypted cookie value too short");
    }
    let nonce = Nonce::from_slice(&encrypted[PREFIX..PREFIX + NONCE]);
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| anyhow::anyhow!("invalid AES-256 key length"))?;
    let plaintext = cipher
        .decrypt(nonce, &encrypted[PREFIX + NONCE..])
        .map_err(|_| anyhow::anyhow!("AES-GCM decryption failed"))?;

    String::from_utf8(plaintext).context("cookie is not valid UTF-8")
}

/// macOS: AES-128-CBC. Format: "v10" (3 B) + ciphertext (PKCS7-padded).
/// IV is always 16 space bytes (0x20). Key is 16 bytes derived via PBKDF2.
#[cfg(target_os = "macos")]
fn chrome_decrypt_cbc(encrypted: &[u8], key: &[u8]) -> Result<String> {
    use aes::Aes128;
    use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};

    type Aes128CbcDec = cbc::Decryptor<Aes128>;

    const IV: [u8; 16] = [0x20u8; 16]; // 16 space characters

    if encrypted.len() < 3 {
        anyhow::bail!("encrypted cookie value too short");
    }

    let key16: &[u8; 16] = key
        .try_into()
        .map_err(|_| anyhow::anyhow!("CBC key must be 16 bytes (got {})", key.len()))?;

    let mut buf = encrypted[3..].to_vec();
    let decrypted = Aes128CbcDec::new(key16.into(), &IV.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| anyhow::anyhow!("AES-128-CBC PKCS7 decryption failed"))?;

    String::from_utf8(decrypted.to_vec())
        .context("cookie is not valid UTF-8")
}

// ─────────────────────────────────────────────────────────────────────────────
// Windows DPAPI
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod dpapi {
    use anyhow::Result;
    use std::ptr;

    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptUnprotectData(
            p_data_in: *const DataBlob,
            pp_sz_descr: *mut *mut u16,
            p_entropy: *const DataBlob,
            pv_reserved: *mut core::ffi::c_void,
            p_prompt: *const core::ffi::c_void,
            dw_flags: u32,
            p_data_out: *mut DataBlob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(h_mem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }

    pub fn decrypt(data: &[u8]) -> Result<Vec<u8>> {
        let input = DataBlob {
            cb_data: data.len() as u32,
            pb_data: data.as_ptr() as *mut u8,
        };
        let mut output = DataBlob {
            cb_data: 0,
            pb_data: ptr::null_mut(),
        };

        let ok = unsafe {
            CryptUnprotectData(
                &input as *const DataBlob,
                ptr::null_mut(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null(),
                0,
                &mut output as *mut DataBlob,
            )
        };

        if ok == 0 {
            anyhow::bail!(
                "DPAPI decryption failed — make sure you are logged in as the same \
                 Windows user who runs Chrome"
            );
        }

        let decrypted =
            unsafe { std::slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec() };
        unsafe { LocalFree(output.pb_data as *mut core::ffi::c_void) };
        Ok(decrypted)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Open the Chrome cookie database, returning a connection.
/// Tries three strategies in order:
///   1. Open the original file directly via SQLite (SQLite uses different
///      internal flags than std::fs and may succeed where a file copy fails).
///   2. Copy with FILE_SHARE_* flags and open the copy.
///   3. Plain copy (works when Chrome is closed).
/// If all strategies fail on Windows with a sharing violation, surfaces a
/// clear error explaining how to work around Chrome's exclusive lock.
fn open_chrome_db(db_path: &Path) -> Result<Connection> {
    // Strategy 1: direct SQLite open on the original path.
    // SQLite in read-only mode acquires only a shared lock and uses internal
    // CreateFile flags that sometimes bypass Chrome's sharing restrictions.
    if let Ok(conn) = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        return Ok(conn);
    }

    // Strategy 2 & 3: copy then open.
    match try_copy_db(db_path) {
        Ok(tmp) => {
            let conn = Connection::open_with_flags(&tmp, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .context("opening copied Chrome cookie database")?;
            Ok(conn)
        }
        Err(e) => {
            // Check the full error chain for a Windows sharing violation (os error 32).
            // e.to_string() only returns the top-level context message; we need
            // format!("{e:#}") to see all layers including the inner OS error.
            let full = format!("{e:#}");
            if full.contains("os error 32") || full.contains("being used by another process") {
                anyhow::bail!(
                    "Chrome has its cookie database locked (this is a security feature in \
                     modern Chrome on Windows).\n\
                     \n\
                     Work-arounds:\n\
                     \n\
                     Option A — close Chrome, run rj, reopen Chrome:\n\
                     \n\
                     Option B — copy the cookie from Chrome DevTools:\n\
                       1. Press F12 in Chrome\n\
                       2. Application tab → Cookies → your Jenkins URL\n\
                       3. Find JSESSIONID.* and copy its value\n\
                       4. Run:  $env:JENKINS_COOKIE = \"JSESSIONID.xxx=<value>\"\n\
                          Then: rj list"
                );
            }
            Err(e)
        }
    }
}

fn try_copy_db(src: &Path) -> Result<PathBuf> {
    let dst = std::env::temp_dir().join("rj_browser_cookies.sqlite");
    copy_file_shared(src, &dst)
        .with_context(|| format!("copying cookie database from '{}'", src.display()))?;

    // Copy WAL and SHM auxiliary files so SQLite sees un-checkpointed writes.
    let src_str = src.as_os_str();
    for suffix in ["-wal", "-shm"] {
        let mut aux_src = src_str.to_owned();
        aux_src.push(suffix);
        let src_aux = Path::new(&aux_src);
        if src_aux.exists() {
            let mut aux_dst = dst.as_os_str().to_owned();
            aux_dst.push(suffix);
            copy_file_shared(src_aux, Path::new(&aux_dst)).ok();
        }
    }
    Ok(dst)
}

/// Copy a file that may be held open by another process.
/// On Windows uses FILE_SHARE_READ|WRITE|DELETE; falls back to plain copy elsewhere.
fn copy_file_shared(src: &Path, dst: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::io::{Read, Write};
        use std::os::windows::fs::OpenOptionsExt;
        const SHARE_ALL: u32 = 0x0000_0007; // READ | WRITE | DELETE
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(SHARE_ALL)
            .open(src)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        std::fs::File::create(dst)?.write_all(&buf)?;
        return Ok(());
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::fs::copy(src, dst)?;
        Ok(())
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn require_nonempty(cookies: Vec<String>, hostname: &str) -> Result<String> {
    if cookies.is_empty() {
        anyhow::bail!(
            "no valid session cookies found for '{}' in your browser.\n\
             Make sure you are logged into Jenkins in the browser, then retry.",
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

    // ── parse_default_profile (Firefox) ──────────────────────────────────────

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
    fn parse_default_profile_picks_correct_when_multiple_profiles() {
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

    // ── chrome_decrypt ────────────────────────────────────────────────────────

    // Windows: AES-256-GCM
    #[cfg(target_os = "windows")]
    #[test]
    fn chrome_decrypt_gcm_roundtrip() {
        use aes_gcm::{
            aead::{Aead, KeyInit, OsRng},
            Aes256Gcm,
        };
        let key = Aes256Gcm::generate_key(OsRng);
        let cipher = Aes256Gcm::new(&key);
        let nonce_bytes = [1u8; 12];
        let nonce = aes_gcm::Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher.encrypt(nonce, b"JSESSIONID-value".as_ref()).unwrap();
        let mut wire = b"v10".to_vec();
        wire.extend_from_slice(&nonce_bytes);
        wire.extend_from_slice(&ciphertext);
        assert_eq!(chrome_decrypt(&wire, &key).unwrap(), "JSESSIONID-value");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn chrome_decrypt_gcm_rejects_too_short_input() {
        assert!(chrome_decrypt(b"v10short", &vec![0u8; 32]).is_err());
    }

    // macOS: AES-128-CBC
    #[cfg(target_os = "macos")]
    #[test]
    fn chrome_decrypt_cbc_roundtrip() {
        use aes::Aes128;
        use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        type Aes128CbcEnc = cbc::Encryptor<Aes128>;

        let key = [0x42u8; 16];
        let iv  = [0x20u8; 16];
        let ciphertext = Aes128CbcEnc::new(&key.into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(b"JSESSIONID-value");

        let mut wire = b"v10".to_vec();
        wire.extend_from_slice(&ciphertext);

        assert_eq!(chrome_decrypt_cbc(&wire, &key).unwrap(), "JSESSIONID-value");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn chrome_decrypt_cbc_rejects_too_short_input() {
        assert!(chrome_decrypt_cbc(b"v1", &[0u8; 16]).is_err());
    }
}
