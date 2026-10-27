// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Various utilities.

use std::{fs, fmt, path::Path, str::FromStr};
#[cfg(unix)]
use std::time::Duration;
use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
#[cfg(unix)]
use dbus::blocking::Connection;
use md5::{Md5, Digest};
#[cfg(unix)]
use nix::{sys::statvfs, unistd::gethostname};
use once_cell::sync::Lazy;
use serde::{Deserialize, Deserializer, Serializer, de::Error};

/// A short, stable fingerprint of a secret value (e.g. the XMR CMS
/// key), safe to log or display -- lets two values be compared to
/// tell whether they're the *same* secret, without ever printing the
/// actual value itself. Shared (not private to xmr.rs) so
/// PlayerSettings's own Debug impl (config.rs) can reuse it too.
pub fn fingerprint(secret: &str) -> String {
    hex::encode(Md5::digest(secret.as_bytes()))[..8].to_string()
}

/// Common time format used by the CMS.
pub static TIME_FMT: Lazy<Vec<time::format_description::FormatItem>> = Lazy::new(|| {
    time::format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second]").unwrap()
});

/// Wrapper to send binary data as Base64 over SOAP.
#[derive(Debug)]
pub struct Base64Field(pub Vec<u8>);

impl fmt::Display for Base64Field {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", BASE64.encode(&self.0))
    }
}

impl FromStr for Base64Field {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(Base64Field(BASE64.decode(s)?))
    }
}


/// Helpers for parsing XML
pub trait ElementExt {
    fn req_attr<'a>(&'a self, attr: &'a str) -> Result<&'a str>;
    fn def_attr<'a>(&'a self, attr: &'a str, def: &'a str) -> &'a str;
    fn parse_attr<T: FromStr>(&self, attr: &str) -> Result<T>
        where T::Err: std::error::Error + Sync + Send + 'static;
    fn req_child<'a>(&'a self, child: &'a str) -> Result<&'a str>;
    fn parse_child<T: FromStr>(&self, child: &str) -> Result<T>
        where T::Err: std::error::Error + Sync + Send + 'static;
    fn def_child<T: FromStr>(&self, child: &str, default: impl Into<T>) -> Result<T>
        where T::Err: std::error::Error + Sync + Send + 'static;
}

impl ElementExt for elementtree::Element {
    fn req_attr<'a>(&'a self, attr: &'a str) -> Result<&'a str> {
        self.get_attr(attr).with_context(|| format!("missing {attr}"))
    }

    fn def_attr<'a>(&'a self, attr: &'a str, def: &'a str) -> &'a str {
        self.get_attr(attr).unwrap_or(def)
    }

    fn parse_attr<T: FromStr>(&self, attr: &str) -> Result<T>
        where T::Err: std::error::Error + Sync + Send + 'static
    {
        self.get_attr(attr).with_context(|| format!("missing {attr}"))?
                           .parse().with_context(|| format!("invalid {attr}"))
    }

    fn req_child<'a>(&'a self, child: &'a str) -> Result<&'a str>
    {
        Ok(self.find(child).with_context(|| format!("missing {child}"))?.text())
    }

    fn parse_child<T: FromStr>(&self, child: &str) -> Result<T>
        where T::Err: std::error::Error + Sync + Send + 'static
    {
        self.find(child).with_context(|| format!("missing {child}"))?
                        .text()
                        .parse().with_context(|| format!("invalid {child}"))
    }

    fn def_child<T: FromStr>(&self, child: &str, default: impl Into<T>) -> Result<T>
        where T::Err: std::error::Error + Sync + Send + 'static
    {
        match self.find(child) {
            None => Ok(default.into()),
            Some(el) => el.text()
                          .parse().with_context(|| format!("invalid {child}"))
        }
    }
}


pub fn percent_decode(s: &str) -> String {
    // Operates on raw bytes throughout (not chars) -- a `%XY` escape
    // can represent any single byte, including one half of a
    // multi-byte UTF-8 sequence (e.g. "%C3%A9" for 'é'); reassembling
    // byte-by-byte and converting to a String only once at the end is
    // what correctly reconstructs those, instead of misinterpreting
    // each decoded byte as its own standalone Unicode codepoint.
    let bytes = s.as_bytes();
    let mut res = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = s.get(i+1..i+3).and_then(|s| u8::from_str_radix(s, 16).ok());
                match hex {
                    Some(b) => { res.push(b); i += 3; }
                    None => { res.push(b'%'); i += 1; }
                }
            }
            b'+' => { res.push(b' '); i += 1; }
            b => { res.push(b); i += 1; }
        }
    }
    String::from_utf8_lossy(&res).into_owned()
}

/// Minimal percent-encoding for a value embedded both in a URL query
/// string and a single-quoted HTML attribute (see the `/local-file`
/// endpoint's own doc comment in server.rs for why this exists at
/// all) -- keeps only a safe ASCII whitelist (letters, digits, and a
/// few common filesystem-path punctuation marks) unescaped, percent-
/// encoding every other byte, including every individual byte of a
/// multi-byte UTF-8 character (never reinterpreting a raw byte as its
/// own `char`, which would otherwise corrupt any non-ASCII character
/// in the original string).
pub fn percent_encode(s: &str) -> String {
    let mut res = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' =>
                res.push(b as char),
            _ => res.push_str(&format!("%{b:02X}")),
        }
    }
    res
}


/// (De)serializing bytestrings for JSON
pub fn ser_hex<S: Serializer>(v: &[u8], s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&hex::encode(v))
}

/// (De)serializing bytestrings for JSON
pub fn de_hex<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<u8>, D::Error> {
    let s = <String as Deserialize>::deserialize(d)?;
    hex::decode(s).map_err(|_| D::Error::custom("invalid hex string"))
}


/// Retrieve MAC address of a system interface.
pub fn retrieve_mac() -> Option<String> {
    for entry in fs::read_dir("/sys/class/net").ok()? {
        let path = entry.ok()?.path();
        // addr_assign_type 0 means that it is an actual permanent address.
        if let Ok("0\n" | "3\n") = fs::read_to_string(path.join("addr_assign_type")).as_deref() {
            if let Ok("1\n") = fs::read_to_string(path.join("carrier")).as_deref() {
                if let Ok(addr) = fs::read_to_string(path.join("address")) {
                    if !addr.ends_with(":00:00\n") {
                        return Some(addr.trim().into());
                    }
                }
            }
        }
    }
    None
}

/// Generate a display ID.  Tries /etc/machine-id, the DMI board id, the MAC or the hostname.
pub fn get_display_id() -> String {
    if let Ok(id) = fs::read_to_string("/etc/machine-id") {
        return id.trim().into();
    }
    // Try the DMI board id, the MAC address and the hostname.
    // Process all info into a big string and hash it.
    let idstring = format!(
        "{:?}{:?}{:?}{:?}",
        fs::read_to_string("/sys/devices/virtual/dmi/id/board_name"),
        fs::read_to_string("/sys/devices/virtual/dmi/id/board_version"),
        retrieve_mac(),
        hostname()
    );
    hex::encode(Md5::digest(idstring.as_bytes()))
}

/// This machine's name.
#[cfg(unix)]
fn hostname() -> Option<String> {
    gethostname().ok().and_then(|s| s.into_string().ok())
}

#[cfg(windows)]
fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

/// Generate an initial display name.  Tries the hostname.
pub fn get_display_name() -> String {
    hostname()
                      .unwrap_or_else(|| "Arexibo Display".into())
}

/// Get this machine's non-loopback IPv4/IPv6 addresses, for display on
/// the splash screen (found genuinely useful during initial totem
/// setup: knowing the machine's own IP without needing to SSH in
/// separately to check, especially while waiting for CMS authorization
/// -- see mainloop.rs's own retry-while-showing-splash logic).
/// Shells out to `hostname -I` rather than reimplementing interface
/// enumeration natively -- `hostname` is part of Ubuntu Server's base
/// install (essential, always present), and this is purely for display,
/// not anything safety/correctness-critical, so a missing/failing
/// command just means an empty result (handled gracefully by the
/// caller), not a hard failure.
pub fn get_local_ips() -> Vec<String> {
    std::process::Command::new("hostname")
        .arg("-I")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

/// Converts a JSON value to a plain string for Schedule Criteria (see
/// xmds::Cms::get_weather's own doc comment) -- matching C#'s own
/// `.ToString()` semantics for a JSON-deserialized value: a JSON
/// string becomes its own unquoted content (not re-serialized with
/// quotes), a number becomes its plain decimal representation, and
/// anything else falls back to its normal JSON text form.
pub fn json_value_to_criteria_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Reads the system's own configured IANA timezone name (e.g.
/// "Europe/Rome") from `/etc/timezone` -- the standard Debian/Ubuntu
/// mechanism, matching this player's own target deployment. Returns
/// None if the file is missing/unreadable (e.g. a non-Debian system,
/// or a container without it set up) -- callers should treat this as
/// "can't verify", not "mismatch".
pub fn read_system_timezone() -> Option<String> {
    std::fs::read_to_string("/etc/timezone").ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}


#[cfg(unix)]
const SS_SVC: &str   = "org.freedesktop.ScreenSaver";
#[cfg(unix)]
const SS_PATH: &str  = "/ScreenSaver";
#[cfg(unix)]
const SS_IFACE: &str = "org.freedesktop.ScreenSaver";
#[cfg(unix)]
const SS_METH: &str  = "Inhibit";

/// Inhibit the screensaver.
#[cfg(unix)]
pub fn inhibit_screensaver() -> Result<u32> {
    let conn = Connection::new_session().context("connecting to dbus")?;
    let proxy = conn.with_proxy(SS_SVC, SS_PATH, Duration::from_millis(500));
    let res: (u32,) = proxy.method_call(SS_IFACE, SS_METH, ("Arexibo", "Showing signage"))?;
    Ok(res.0)
}

/// Keeps the display and the system awake for as long as the calling thread
/// lives: called once, from the main thread, which is also the GUI's.
#[cfg(windows)]
pub fn inhibit_screensaver() -> Result<u32> {
    use windows_sys::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
    };
    // SAFETY: a plain call that takes flags.
    let previous = unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED)
    };
    anyhow::ensure!(previous != 0, "SetThreadExecutionState failed");
    Ok(0)
}


/// Get available and total space in directory.
#[cfg(unix)]
pub fn space_info(path: &Path) -> Result<(u64, u64)> {
    let res = statvfs::statvfs(path)?;
    Ok((res.blocks_available() * res.fragment_size(),
        res.blocks() * res.fragment_size()))
}

#[cfg(windows)]
pub fn space_info(path: &Path) -> Result<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is NUL-terminated and the pointers are to live variables.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free) };
    anyhow::ensure!(ok != 0, "GetDiskFreeSpaceExW: {}", std::io::Error::last_os_error());
    Ok((available, total))
}

/// Get current IANA timezone name ("Europe/Berlin").
pub fn timezone() -> String {
    // try /etc/timezone which should have the name
    if let Ok(zone) = fs::read_to_string("/etc/timezone") {
        return zone.trim().into();
    }
    // otherwise, /etc/localtime should be a symlink to a zoneinfo file
    else if let Ok(tgt) = fs::read_link("/etc/localtime") {
        let path = tgt.to_string_lossy();
        if let Some(pos) = path.find("/zoneinfo/") {
            return path[pos + "/zoneinfo/".len()..].into();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_string_value_becomes_unquoted_criteria_string() {
        // Matches C#'s own .ToString() semantics -- a JSON string
        // becomes its own content, not re-serialized with quotes
        // (serde_json::Value::to_string() would otherwise produce
        // "\"clear\"" instead of "clear").
        let v: serde_json::Value = serde_json::from_str(r#""clear""#).unwrap();
        assert_eq!(json_value_to_criteria_string(&v), "clear");
    }

    #[test]
    fn json_number_value_becomes_plain_decimal_string() {
        let v: serde_json::Value = serde_json::from_str("25").unwrap();
        assert_eq!(json_value_to_criteria_string(&v), "25");
        let v: serde_json::Value = serde_json::from_str("18.5").unwrap();
        assert_eq!(json_value_to_criteria_string(&v), "18.5");
    }

    #[test]
    fn fingerprint_is_deterministic_for_the_same_secret() {
        // The whole point: comparing two log lines/dumps from
        // different points in time must reliably tell whether the
        // *same* secret was used both times.
        assert_eq!(fingerprint("mysecretkey"), fingerprint("mysecretkey"));
    }

    #[test]
    fn fingerprint_differs_for_different_secrets() {
        assert_ne!(fingerprint("mysecretkey"), fingerprint("adifferentkey"));
    }

    #[test]
    fn fingerprint_never_reveals_the_secret_itself() {
        // Genuinely important, not just a nice-to-have: this gets
        // logged/displayed, so the actual secret must never appear
        // verbatim (or as an obvious substring) in the output.
        let secret = "supersecretxmrcmskey12345";
        let fp = fingerprint(secret);
        assert!(!fp.contains(secret));
        assert_eq!(fp.len(), 8, "expected a short, fixed-length fingerprint");
    }

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
    }

    #[test]
    fn percent_decode_plus() {
        assert_eq!(percent_decode("hello+world"), "hello world");
    }

    #[test]
    fn percent_decode_no_encoding() {
        assert_eq!(percent_decode("hello"), "hello");
    }

    #[test]
    fn percent_decode_special_chars() {
        assert_eq!(percent_decode("%26amp%3B"), "&amp;");
    }

    #[test]
    fn percent_decode_empty() {
        assert_eq!(percent_decode(""), "");
    }

    #[test]
    fn percent_decode_mixed() {
        assert_eq!(percent_decode("a%20b+c%21d"), "a b c!d");
    }

    #[test]
    fn percent_decode_real_shellcommand_from_user_log() {
        // Regression test using the exact values from a real log the
        // user shared, confirming a genuine bug: these were never
        // decoded before being logged or run.
        assert_eq!(
            percent_decode("%2Fusr%2Fbin%2Ftouch+%2Ftmp%2Fxibo-adhoc-test"),
            "/usr/bin/touch /tmp/xibo-adhoc-test"
        );
    }

    #[test]
    fn percent_decode_http_command() {
        assert_eq!(
            percent_decode(
                "http%7Chttp%3A%2F%2F192.0.2.10%3A8888%2Fping%7Capplication%2Fjson%7C\
                 %7B%22method%22%3A%22GET%22%2C%22headers%22%3A%22%7B%7D%22%2C%22body%22%3A%22%7B%7D%22%7D"
            ),
            r#"http|http://192.0.2.10:8888/ping|application/json|{"method":"GET","headers":"{}","body":"{}"}"#
        );
    }

    #[test]
    fn percent_encode_leaves_plain_paths_alone() {
        assert_eq!(percent_encode("/home/user/arexibo-test/res/148.mp4"),
                   "/home/user/arexibo-test/res/148.mp4");
    }

    #[test]
    fn percent_encode_escapes_query_and_attribute_breaking_chars() {
        assert_eq!(percent_encode("/a b/c&d=e'f\"g<h>i.mp4"),
                   "/a%20b/c%26d%3De%27f%22g%3Ch%3Ei.mp4");
    }

    #[test]
    fn percent_encode_handles_multibyte_utf8_without_corruption() {
        // A raw-byte-as-char bug here would corrupt any non-ASCII
        // character in the original string -- round-tripping through
        // percent_decode must reproduce it exactly.
        let original = "/home/usuário/vidéo.mp4";
        let encoded = percent_encode(original);
        assert_eq!(percent_decode(&encoded), original);
    }

    #[test]
    fn base64_field_roundtrip() {
        let data = vec![1, 2, 3, 255, 0];
        let field = Base64Field(data.clone());
        let encoded = field.to_string();
        let decoded: Base64Field = encoded.parse().unwrap();
        assert_eq!(decoded.0, data);
    }

    #[test]
    fn base64_field_empty() {
        let field = Base64Field(vec![]);
        let encoded = field.to_string();
        assert_eq!(encoded, "");
        let decoded: Base64Field = encoded.parse().unwrap();
        assert_eq!(decoded.0, Vec::<u8>::new());
    }

    #[test]
    fn hex_roundtrip() {
        let original = [0xDE, 0xAD, 0xBE, 0xEF];
        let json = serde_json::to_string(&hex::encode(original)).unwrap();
        let back: String = serde_json::from_str(&json).unwrap();
        assert_eq!(hex::decode(back).unwrap(), original);
    }
}

/// The folder of the player's files when none is given: `Documents\arexibo`
/// on Windows (the real Documents folder, which OneDrive or the language may
/// have moved); nothing elsewhere.
#[cfg(windows)]
pub fn default_envdir() -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath},
    };
    let mut raw: *mut u16 = std::ptr::null_mut();
    // SAFETY: the call writes `raw`; when it is not null it points to a
    // NUL-terminated string, which has to be freed with CoTaskMemFree.
    let documents = unsafe {
        let result = SHGetKnownFolderPath(&FOLDERID_Documents, 0, 0, &mut raw);
        let documents = (result >= 0 && !raw.is_null()).then(|| {
            let len = (0..).take_while(|&i| *raw.add(i) != 0).count();
            std::ffi::OsString::from_wide(std::slice::from_raw_parts(raw, len))
        });
        CoTaskMemFree(raw as *const _);
        documents
    };
    documents
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|home| std::path::PathBuf::from(home).join("Documents")))
        .map(|dir| dir.join("arexibo"))
}

#[cfg(not(windows))]
pub fn default_envdir() -> Option<std::path::PathBuf> {
    None
}

/// The folder of the player's files: the one given, which has to exist, or
/// the default one, which is made if it is missing.
pub fn resolve_envdir(given: Option<std::path::PathBuf>, default: Option<std::path::PathBuf>)
                      -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context as _;
    if let Some(dir) = given {
        anyhow::ensure!(dir.exists(), "environment directory '{}' does not exist", dir.display());
        return Ok(dir);
    }
    let dir = default.context("no environment directory was given (the first argument), \
                               and there is no default one on this system")?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating the environment directory '{}'", dir.display()))?;
    log::info!("no environment directory given: using {}", dir.display());
    Ok(dir)
}

#[cfg(test)]
mod envdir_tests {
    use super::*;
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("arexibo_envdir_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_given_folder_that_is_there_is_used_as_it_is() {
        let dir = temp("given");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(resolve_envdir(Some(dir.clone()), Some(temp("unused"))).unwrap(), dir);
        assert!(!temp("unused").exists(), "the default is left alone");
    }

    #[test]
    fn a_given_folder_that_is_missing_is_an_error_not_made() {
        let dir = temp("missing");
        let message = format!("{:#}", resolve_envdir(Some(dir.clone()), Some(temp("unused2"))).unwrap_err());
        assert!(message.contains("does not exist"), "{message}");
        assert!(!dir.exists(), "a mistyped path is not turned into a folder");
    }

    #[test]
    fn the_default_folder_is_made_when_it_is_missing_with_its_parents() {
        let dir = temp("default").join("deep").join("arexibo");
        assert_eq!(resolve_envdir(None, Some(dir.clone())).unwrap(), dir);
        assert!(dir.is_dir());
    }

    #[test]
    fn the_default_folder_is_kept_as_it_is_when_it_is_there() {
        let dir = temp("kept");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cms.json"), "{}").unwrap();
        assert_eq!(resolve_envdir(None, Some(dir.clone())).unwrap(), dir);
        assert_eq!(std::fs::read_to_string(dir.join("cms.json")).unwrap(), "{}");
    }

    #[test]
    fn with_no_folder_and_no_default_it_says_what_is_needed() {
        let message = format!("{:#}", resolve_envdir(None, None).unwrap_err());
        assert!(message.contains("no environment directory was given"), "{message}");
    }

    #[test]
    fn a_default_that_cannot_be_made_is_an_error() {
        let file = temp("a_file");
        std::fs::write(&file, "x").unwrap();
        let message = format!("{:#}", resolve_envdir(None, Some(file.join("arexibo"))).unwrap_err());
        assert!(message.contains("creating the environment directory"), "{message}");
        let _ = std::fs::remove_file(&file);
    }

    #[cfg(not(windows))]
    #[test]
    fn there_is_no_default_outside_windows() {
        assert_eq!(default_envdir(), None);
    }
}
