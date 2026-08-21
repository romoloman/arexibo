// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Various utilities.

use std::{fs, fmt, path::Path, str::FromStr, time::Duration};
use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use dbus::blocking::Connection;
use md5::{Md5, Digest};
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
    let mut res = String::new();
    let mut iter = s.char_indices();
    while let Some((i, ch)) = iter.next() {
        match ch {
            '%' => {
                let codepoint = s.get(i+1..i+3)
                                 .and_then(|s| u8::from_str_radix(s, 16).ok());
                if let Some(hex) = codepoint {
                    res.push(hex as char);
                    iter.nth(1);
                }
            },
            '+' => res.push(' '),
            _ => res.push(ch),
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
        gethostname().ok().and_then(|s| s.into_string().ok())
    );
    hex::encode(Md5::digest(idstring.as_bytes()))
}

/// Generate an initial display name.  Tries the hostname.
pub fn get_display_name() -> String {
    gethostname().ok().and_then(|s| s.into_string().ok())
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


const SS_SVC: &str   = "org.freedesktop.ScreenSaver";
const SS_PATH: &str  = "/ScreenSaver";
const SS_IFACE: &str = "org.freedesktop.ScreenSaver";
const SS_METH: &str  = "Inhibit";

/// Inhibit the screensaver.
pub fn inhibit_screensaver() -> Result<u32> {
    let conn = Connection::new_session().context("connecting to dbus")?;
    let proxy = conn.with_proxy(SS_SVC, SS_PATH, Duration::from_millis(500));
    let res: (u32,) = proxy.method_call(SS_IFACE, SS_METH, ("Arexibo", "Showing signage"))?;
    Ok(res.0)
}


/// Get available and total space in directory.
pub fn space_info(path: &Path) -> Result<(u64, u64)> {
    let res = statvfs::statvfs(path)?;
    Ok((res.blocks_available() * res.fragment_size(),
        res.blocks() * res.fragment_size()))
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
    fn percent_decode_real_http_command_from_user_log() {
        assert_eq!(
            percent_decode(
                "http%7Chttp%3A%2F%2F192.168.0.245%3A8888%2Fping%7Capplication%2Fjson%7C\
                 %7B%22method%22%3A%22GET%22%2C%22headers%22%3A%22%7B%7D%22%2C%22body%22%3A%22%7B%7D%22%7D"
            ),
            r#"http|http://192.168.0.245:8888/ping|application/json|{"method":"GET","headers":"{}","body":"{}"}"#
        );
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
        let original = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let json = serde_json::to_string(&hex::encode(&original)).unwrap();
        let back: String = serde_json::from_str(&json).unwrap();
        assert_eq!(hex::decode(back).unwrap(), original);
    }
}
