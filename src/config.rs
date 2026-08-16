// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Definitions for the player configuration.

use std::{collections::HashMap, fmt, fs::File, path::Path, sync::Arc, time::Duration};
use anyhow::{Context, Result};
use md5::{Md5, Digest};
use serde::{Serialize, Deserialize};
use rustls::client::danger;
use rustls::crypto::aws_lc_rs;
use rustls_pki_types::CertificateDer;
use crate::command::Command;
use crate::util::fingerprint;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PlayerSettings {
    #[serde(default = "default_collect_interval")]
    pub collect_interval: u64,
    #[serde(default)]
    pub stats_enabled: bool,
    #[serde(default)]
    pub xmr_network_address: String,
    #[serde(default)]
    pub xmr_web_socket_address: String,
    #[serde(default)]
    pub xmr_cms_key: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub screenshot_interval: u64,
    // Max width (px) to downscale screenshots to before submission, 0 =
    // no resize (send at full captured resolution) -- confirmed real
    // setting from the C# client's own default.config.xml
    // (`<ScreenShotSize>0</ScreenShotSize>`), previously not read/
    // respected at all here.
    #[serde(default)]
    pub screenshot_size: u32,
    // Master on/off switch for Adspace Exchange (`ssp` widgets, see
    // adspace.rs) -- confirmed real field name from the actual C#
    // client source seen during development
    // (`ApplicationSettings.Default.IsAdspaceEnabled`). Defaults to
    // false (fails closed: no Adspace Exchange network requests unless
    // the CMS explicitly turns it on for this display).
    #[serde(default)]
    pub is_adspace_enabled: bool,
    // BUG fix (found from a real report, confirmed via a real
    // RegisterDisplay XML response the user shared): these are
    // "HH:MM" strings (e.g. "12:00"/"15:00"), NOT plain integers --
    // an earlier version of this code wrongly assumed a bare hour
    // number based on a *different*, local config-file representation
    // (a real XiboClient.config dump showing
    // `<DownloadStartWindow>0</DownloadStartWindow>`), which turned out
    // to not match the actual XMDS wire format at all. See
    // `PlayerSettings::is_within_download_window` for how these
    // actually get enforced.
    #[serde(default)]
    pub download_start_window: String,
    #[serde(default)]
    pub download_end_window: String,
    #[serde(default = "default_embedded_server_port")]
    pub embedded_server_port: u16,
    #[serde(default)]
    pub prevent_sleep: bool,
    #[serde(default = "default_display_name")]
    pub display_name: String,
    #[serde(default)]
    pub size_x: i32,
    #[serde(default)]
    pub size_y: i32,
    #[serde(default)]
    pub pos_x: i32,
    #[serde(default)]
    pub pos_y: i32,
    #[serde(default)]
    pub commands: HashMap<String, Command>,
    // Security: master on/off switch for shell/command execution
    // capability, mirroring the C# client's `EnableShellCommands`
    // (confirmed default `false` from a real exported XiboClient.config
    // on the community forum). Defaults to `false` here too -- fail
    // closed if the CMS response is somehow missing this field, rather
    // than silently allowing arbitrary command execution.
    #[serde(default)]
    pub enable_shell_commands: bool,
    // Comma-separated allowlist (`ShellCommandAllowList` in the C#
    // client) restricting which commands a Layout's own shellcommand
    // widget (`run_shell` in mainloop.rs -- an arbitrary command line
    // embedded directly in Layout content, the actually risky vector)
    // may run. Empty means "no restriction beyond enable_shell_commands
    // itself" -- matches the observed default (`<ShellCommandAllowList
    // />`, i.e. empty). Does NOT restrict `run_command` (CMS Display
    // Profile-preregistered commands, triggered by *selecting* one via a
    // `commandCode`/`commandAction` -- already vetted by being centrally
    // configured, so only gated by enable_shell_commands itself).
    #[serde(default)]
    pub shell_command_allow_list: String,
}

/// Manual `Debug` impl -- deliberately *not* derived. `xmr_cms_key` is
/// a secret credential (see its own doc comment and xmr.rs's own
/// `fingerprint` doc comment for the full context); a naive derived
/// Debug would print it in clear text through any `{:?}`/log::debug!
/// dump anywhere in the codebase, now or in the future, without
/// whoever writes that call site necessarily remembering to redact it
/// specifically. Implementing this by hand instead makes that
/// mistake structurally impossible -- every field prints normally
/// except this one, which always shows a short fingerprint (safe to
/// compare across two dumps, e.g. to answer "did the CMS send the
/// same key both times") instead of the actual secret.
impl fmt::Debug for PlayerSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlayerSettings")
            .field("collect_interval", &self.collect_interval)
            .field("stats_enabled", &self.stats_enabled)
            .field("xmr_network_address", &self.xmr_network_address)
            .field("xmr_web_socket_address", &self.xmr_web_socket_address)
            .field("xmr_cms_key", &format_args!("<redacted, fingerprint {}>",
                                                 fingerprint(&self.xmr_cms_key)))
            .field("log_level", &self.log_level)
            .field("screenshot_interval", &self.screenshot_interval)
            .field("screenshot_size", &self.screenshot_size)
            .field("is_adspace_enabled", &self.is_adspace_enabled)
            .field("download_start_window", &self.download_start_window)
            .field("download_end_window", &self.download_end_window)
            .field("embedded_server_port", &self.embedded_server_port)
            .field("prevent_sleep", &self.prevent_sleep)
            .field("display_name", &self.display_name)
            .field("size_x", &self.size_x)
            .field("size_y", &self.size_y)
            .field("pos_x", &self.pos_x)
            .field("pos_y", &self.pos_y)
            .field("commands", &self.commands)
            .field("enable_shell_commands", &self.enable_shell_commands)
            .field("shell_command_allow_list", &self.shell_command_allow_list)
            .finish()
    }
}

impl PlayerSettings {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        serde_json::from_reader(File::open(path.as_ref())?)
            .context("deserializing player settings")
    }

    pub fn to_file(&self, path: impl AsRef<Path>) -> Result<()> {
        serde_json::to_writer_pretty(File::create(path.as_ref())?, self)
            .context("serializing player settings")
    }

    /// Whether bulk file downloads (media/resources/layouts -- NOT the
    /// lightweight RegisterDisplay/Schedule/RequiredFiles metadata calls
    /// themselves, which should keep happening regardless so the
    /// schedule stays current) are currently allowed, per the CMS's own
    /// `DownloadStartWindow`/`DownloadEndWindow` Display Profile setting
    /// -- a way to keep a display from hogging bandwidth during
    /// business hours. BUG fix (found from a real report): this setting
    /// was parsed nowhere and enforced nowhere at all before -- see
    /// `mainloop.rs`'s `collect_once` for where this is actually applied
    /// now.
    ///
    /// `start == end` (0/0 is the common CMS default when the feature
    /// isn't actively configured) means "no restriction" -- a literal
    /// zero-width window would otherwise permanently block every
    /// download, which is certainly not the intent of an unconfigured
    /// setting. `start > end` is a legitimate overnight window (e.g.
    /// 22-6, meaning 22:00 through 06:00 the next day) -- checked as a
    /// wraparound rather than assuming `start <= end`.
    /// Whether bulk file downloads (media/resources/layouts -- NOT the
    /// lightweight RegisterDisplay/Schedule/RequiredFiles metadata calls
    /// themselves, which should keep happening regardless so the
    /// schedule stays current) are currently allowed, per the CMS's own
    /// `DownloadStartWindow`/`DownloadEndWindow` Display Profile setting
    /// -- a way to keep a display from hogging bandwidth during
    /// business hours. BUG fix (found from a real report): this setting
    /// was parsed nowhere and enforced nowhere at all before -- see
    /// `mainloop.rs`'s `collect_once` for where this is actually applied
    /// now.
    ///
    /// Either field being empty, missing, or unparseable as `"HH:MM"`
    /// (confirmed real wire format from an actual RegisterDisplay
    /// response) means "no restriction" -- fails open rather than
    /// blocking every download just because this setting wasn't
    /// configured or came back in some unexpected shape. `start == end`
    /// (parsed to the same minute-of-day) is treated the same way -- a
    /// literal zero-width window would otherwise permanently block
    /// every download, which is certainly not the intent of an
    /// unconfigured setting.
    pub fn is_within_download_window(&self) -> bool {
        let (Some(start), Some(end)) = (
            Self::parse_hhmm(&self.download_start_window),
            Self::parse_hhmm(&self.download_end_window),
        ) else {
            return true;
        };
        let now_minute = time::OffsetDateTime::now_local()
            .map(|t| t.hour() as u16 * 60 + t.minute() as u16)
            .unwrap_or(0);
        Self::minute_in_download_window(now_minute, start, end)
    }

    /// Parses a `"HH:MM"` string (the confirmed real wire format for
    /// `downloadStartWindow`/`downloadEndWindow`) into minutes since
    /// midnight, or `None` if empty/malformed/out of range.
    fn parse_hhmm(s: &str) -> Option<u16> {
        let (h, m) = s.trim().split_once(':')?;
        let h: u16 = h.parse().ok()?;
        let m: u16 = m.parse().ok()?;
        (h < 24 && m < 60).then_some(h * 60 + m)
    }

    /// Pure logic behind `is_within_download_window`, split out purely
    /// so it can be unit-tested against specific times directly instead
    /// of depending on the real wall-clock time the test happens to run
    /// at. All three arguments are minutes since midnight (0-1439).
    fn minute_in_download_window(now_minute: u16, start: u16, end: u16) -> bool {
        if start == end {
            return true;
        }
        if start < end {
            now_minute >= start && now_minute < end
        } else {
            // Overnight wraparound: e.g. 22:00-06:00 -> allowed from
            // 22:00 through 23:59, then again from 00:00 through 05:59.
            now_minute >= start || now_minute < end
        }
    }

    /// Maps the CMS's own `logLevel` Display Profile setting (a string,
    /// e.g. "error"/"warn"/"info"/"debug"/"audit") to the equivalent
    /// Rust `log::LevelFilter`. BUG fix (found from a real report --
    /// cross-checking another fork's own overnight-audit findings,
    /// which flagged a *mapping* bug here; on inspection, this codebase's
    /// gap was more fundamental than a wrong mapping: `log_level` was
    /// parsed from the CMS's response into `PlayerSettings` but never
    /// actually applied anywhere at all -- only the local `--debug` CLI
    /// flag affected the real log verbosity, regardless of what the
    /// CMS's own Display Profile said). Falls back to `Info` for any
    /// unrecognized value, rather than silently doing nothing.
    pub fn log_level_filter(&self) -> log::LevelFilter {
        match self.log_level.to_lowercase().as_str() {
            "error" => log::LevelFilter::Error,
            "warn" | "warning" => log::LevelFilter::Warn,
            "info" | "audit" => log::LevelFilter::Info,
            "debug" => log::LevelFilter::Debug,
            "trace" => log::LevelFilter::Trace,
            _ => log::LevelFilter::Info,
        }
    }
}

fn default_collect_interval() -> u64 { 900 }
fn default_log_level() -> String { "debug".into() }
fn default_embedded_server_port() -> u16 { 9696 }
fn default_display_name() -> String { "Xibo".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmsSettings {
    pub address: String,
    pub key: String,
    pub display_id: String,
    pub display_name: Option<String>,
    pub proxy: Option<String>,
}

impl CmsSettings {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        serde_json::from_reader(File::open(path.as_ref())?)
            .context("deserializing player settings")
    }

    pub fn to_file(&self, path: impl AsRef<Path>) -> Result<()> {
        serde_json::to_writer_pretty(File::create(path.as_ref())?, self)
            .context("serializing player settings")
    }

    /// Deterministic XMR channel ID: `MD5(address + key + display_id)`
    pub fn xmr_channel(&self) -> String {
        let to_hash = format!("{}{}{}", self.address, self.key, self.display_id);
        hex::encode(Md5::digest(to_hash))
    }

    pub fn make_agent(&self, no_verify: bool) -> Result<ureq::Agent> {
        let tls_config = ureq::tls::TlsConfig::builder()
            .disable_verification(no_verify)
            .build();
        let proxy = if let Some(proxy) = &self.proxy {
            Some(ureq::Proxy::new(proxy)?)
        } else {
            None
        };
        Ok(ureq::config::Config::builder()
            .timeout_connect(Some(Duration::from_secs(3)))
            // BUG fix (found from a real report: after several
            // "Cache not ready" SOAP faults and retries -- section 33
            // -- the whole player appeared to stop responding to
            // *everything*, not just dataset updates: no more XMR
            // messages processed, no more periodic layout refresh
            // either). `timeout_connect` alone only bounds how long
            // establishing the connection itself can take -- once
            // connected, if the CMS is slow to respond (plausibly
            // exactly when it's already struggling enough to return
            // "Cache not ready" in the first place) or the connection
            // simply hangs after that point, ureq would wait
            // indefinitely for a response, with no timeout at all
            // protecting against it. Since every network call this
            // agent makes runs synchronously on the mainloop's own
            // single thread (shared with XMR message handling and the
            // periodic schedule_check tick, see mainloop.rs's `run()`
            // select! loop), one such hang blocks literally everything
            // else the player does, indefinitely -- exactly the
            // reported symptom. `timeout_global` is end-to-end (DNS
            // lookup through finishing reading the response body) and
            // therefore covers this case regardless of *where* in the
            // request lifecycle something goes wrong; 30s is generous
            // enough for a normal SOAP round-trip (including a slow
            // GetResource render) while still bounding the worst case
            // to something the player can recover from on its own.
            .timeout_global(Some(Duration::from_secs(30)))
            .tls_config(tls_config)
            .proxy(proxy)
            .build().into())
    }

    pub fn make_rustls_client_config(&self, no_verify: bool) -> Result<rustls::ClientConfig> {
        // BUG fix (found from a real crash report on GitHub: "Player now
        // freeze on splash screen", panicking at this exact line).
        // `install_default()` can only ever *succeed* once per process --
        // this function gets called every time `xmr::start()` runs
        // (src/xmr.rs), and that happens not just once at startup but
        // also from the `--allow-offline` retry path (mainloop.rs,
        // section 50's own fix: if the initial XMR setup fails and
        // `--allow-offline` is set, a retry happens later once network
        // connectivity is confirmed). On that second call, the crypto
        // provider is already installed -- `install_default()` correctly
        // returns `Err(Arc<CryptoProvider>)` in that case (simply handing
        // back the *already-installed* provider, not signaling a genuine
        // failure), but treating that as fatal via `.expect(...)` crashed
        // the entire player the moment a real-world retry actually fired.
        // Ignoring the Err case here is exactly what should happen: some
        // provider (ours or otherwise) is already active either way, and
        // that's perfectly fine for our purposes.
        let _ = aws_lc_rs::default_provider().install_default();
        let mut root_store = rustls::RootCertStore::empty();
        if !no_verify {
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        let mut builder = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        if no_verify {
            builder
                .dangerous()
                .set_certificate_verifier(Arc::new(DisabledVerifier));
        }
        Ok(builder)
    }
}

/// Copied from ureq's rustls impl.
#[derive(Debug)]
struct DisabledVerifier;

impl danger::ServerCertVerifier for DisabledVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<danger::ServerCertVerified, rustls::Error> {
        Ok(danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<danger::HandshakeSignatureValid, rustls::Error> {
        Ok(danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<danger::HandshakeSignatureValid, rustls::Error> {
        Ok(danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA1,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::ED448,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_impl_never_reveals_the_xmr_cms_key_in_clear() {
        // Requested directly: a debug option to print the settings
        // received from the CMS in full, to help investigate exactly
        // this kind of issue. Must never leak the one genuinely
        // sensitive field in the process.
        let mut s = PlayerSettings::default();
        s.xmr_cms_key = "supersecretxmrcmskey12345".to_string();
        let dump = format!("{s:?}");
        assert!(!dump.contains("supersecretxmrcmskey12345"),
                "the raw XMR CMS key must never appear in a Debug dump -- got: {dump}");
        assert!(dump.contains("redacted"), "should clearly indicate the field was redacted");
    }

    #[test]
    fn debug_impl_still_shows_every_other_field_normally() {
        // The redaction must be surgical -- everything else stays
        // fully visible and useful for troubleshooting, not swept into
        // the same "redacted" treatment as the one sensitive field.
        let mut s = PlayerSettings::default();
        s.xmr_web_socket_address = "ws://192.168.2.138:8080".to_string();
        s.display_name = "Totem Ingresso".to_string();
        s.collect_interval = 123;
        let dump = format!("{s:?}");
        assert!(dump.contains("ws://192.168.2.138:8080"));
        assert!(dump.contains("Totem Ingresso"));
        assert!(dump.contains("123"));
    }

    #[test]
    fn player_settings_defaults() {
        let s: PlayerSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.collect_interval, 900);
        assert_eq!(s.log_level, "debug");
        assert_eq!(s.embedded_server_port, 9696);
        assert_eq!(s.display_name, "Xibo");
        assert!(!s.stats_enabled);
        assert!(!s.prevent_sleep);
        // security: fail closed if these are somehow absent
        assert!(!s.enable_shell_commands);
        assert_eq!(s.shell_command_allow_list, "");
        assert_eq!(s.screenshot_size, 0);
        assert!(!s.is_adspace_enabled);
    }

    #[test]
    fn player_settings_custom_values() {
        let json = r#"{"collect_interval": 60, "stats_enabled": true, "display_name": "Lobby"}"#;
        let s: PlayerSettings = serde_json::from_str(json).unwrap();
        assert_eq!(s.collect_interval, 60);
        assert!(s.stats_enabled);
        assert_eq!(s.display_name, "Lobby");
        // defaults for unspecified fields
        assert_eq!(s.log_level, "debug");
    }

    #[test]
    fn player_settings_roundtrip() {
        let original = PlayerSettings {
            collect_interval: 300,
            stats_enabled: true,
            log_level: "info".into(),
            display_name: "Test Display".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: PlayerSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn cms_settings_xmr_channel_deterministic() {
        let cms = CmsSettings {
            address: "https://cms.example.com".into(),
            key: "secret123".into(),
            display_id: "abc-def".into(),
            display_name: None,
            proxy: None,
        };
        let ch1 = cms.xmr_channel();
        let ch2 = cms.xmr_channel();
        assert_eq!(ch1, ch2);
        assert_eq!(ch1.len(), 32); // MD5 hex = 32 chars
    }

    #[test]
    fn cms_settings_xmr_channel_varies() {
        let cms1 = CmsSettings {
            address: "https://a.com".into(),
            key: "key1".into(),
            display_id: "d1".into(),
            display_name: None,
            proxy: None,
        };
        let cms2 = CmsSettings {
            address: "https://b.com".into(),
            key: "key1".into(),
            display_id: "d1".into(),
            display_name: None,
            proxy: None,
        };
        assert_ne!(cms1.xmr_channel(), cms2.xmr_channel());
    }

    #[test]
    fn cms_settings_roundtrip_json() {
        let cms = CmsSettings {
            address: "https://cms.example.com".into(),
            key: "secret".into(),
            display_id: "xyz".into(),
            display_name: Some("Reception".into()),
            proxy: None,
        };
        let json = serde_json::to_string_pretty(&cms).unwrap();
        let parsed: CmsSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.address, cms.address);
        assert_eq!(parsed.key, cms.key);
        assert_eq!(parsed.display_id, cms.display_id);
        assert_eq!(parsed.display_name, cms.display_name);
    }

    #[test]
    fn download_window_parses_real_hhmm_strings_from_a_real_registerdisplay_response() {
        // Regression test using the exact values from a real
        // RegisterDisplay response the user shared, confirming the
        // wire format really is "HH:MM" strings (an earlier version of
        // this code wrongly assumed a bare integer, based on a
        // different, local config-file representation that doesn't
        // match this at all).
        assert_eq!(PlayerSettings::parse_hhmm("12:00"), Some(12 * 60));
        assert_eq!(PlayerSettings::parse_hhmm("15:00"), Some(15 * 60));
        // The exact scenario from that real response: outside 12:00-15:00.
        assert!(!PlayerSettings::minute_in_download_window(16 * 60, 12 * 60, 15 * 60));
        assert!(PlayerSettings::minute_in_download_window(13 * 60 + 30, 12 * 60, 15 * 60));
    }

    #[test]
    fn download_window_parse_hhmm_handles_minutes_precisely() {
        assert_eq!(PlayerSettings::parse_hhmm("09:05"), Some(9 * 60 + 5));
        assert_eq!(PlayerSettings::parse_hhmm("23:59"), Some(23 * 60 + 59));
        assert_eq!(PlayerSettings::parse_hhmm(""), None, "empty string means unconfigured");
        assert_eq!(PlayerSettings::parse_hhmm("garbage"), None);
        assert_eq!(PlayerSettings::parse_hhmm("24:00"), None, "hour out of range");
        assert_eq!(PlayerSettings::parse_hhmm("12:60"), None, "minute out of range");
    }

    #[test]
    fn download_window_missing_or_empty_means_no_restriction() {
        let s: PlayerSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.download_start_window, "");
        assert_eq!(s.download_end_window, "");
        assert!(s.is_within_download_window(), "unconfigured (empty) window must fail open");
    }

    #[test]
    fn download_window_start_equals_end_means_no_restriction() {
        // Same minute-of-day for both -- must NOT be interpreted as a
        // literal zero-width window that blocks every download
        // permanently.
        for minute in [0, 1, 100, 719, 720, 1439] {
            assert!(PlayerSettings::minute_in_download_window(minute, 540, 540),
                    "minute {minute} should be allowed when start==end");
        }
    }

    #[test]
    fn download_window_normal_range() {
        // 09:00-17:00
        let (start, end) = (9 * 60, 17 * 60);
        assert!(!PlayerSettings::minute_in_download_window(8 * 60 + 59, start, end));
        assert!(PlayerSettings::minute_in_download_window(9 * 60, start, end),
                "start minute is inclusive");
        assert!(PlayerSettings::minute_in_download_window(12 * 60, start, end));
        assert!(PlayerSettings::minute_in_download_window(16 * 60 + 59, start, end));
        assert!(!PlayerSettings::minute_in_download_window(17 * 60, start, end),
                "end minute is exclusive");
        assert!(!PlayerSettings::minute_in_download_window(20 * 60, start, end));
    }

    #[test]
    fn download_window_overnight_wraparound() {
        // 22:00-06:00 (10pm through 6am the next day)
        let (start, end) = (22 * 60, 6 * 60);
        assert!(PlayerSettings::minute_in_download_window(22 * 60, start, end),
                "start minute is inclusive");
        assert!(PlayerSettings::minute_in_download_window(23 * 60, start, end));
        assert!(PlayerSettings::minute_in_download_window(0, start, end));
        assert!(PlayerSettings::minute_in_download_window(5 * 60 + 59, start, end));
        assert!(!PlayerSettings::minute_in_download_window(6 * 60, start, end),
                "end minute is exclusive");
        assert!(!PlayerSettings::minute_in_download_window(12 * 60, start, end),
                "midday should be blocked");
        assert!(!PlayerSettings::minute_in_download_window(21 * 60 + 59, start, end));
    }
}

#[cfg(test)]
mod download_window_unset_variants_tests {
    use super::*;

    #[test]
    fn literal_colon_with_nothing_around_it_means_no_restriction() {
        // Confirmed real: when the download window isn't actively
        // configured, the CMS can send a literal ":" (empty hour, empty
        // minute, just the separator) rather than an empty string
        // entirely.
        assert_eq!(PlayerSettings::parse_hhmm(":"), None,
                    "\":\" alone has no parseable hour or minute");
        let s = PlayerSettings {
            download_start_window: ":".into(),
            download_end_window: ":".into(),
            ..serde_json::from_str("{}").unwrap()
        };
        assert!(s.is_within_download_window(), "\":\" on both sides must fail open");
    }

    #[test]
    fn plain_windows_style_zero_also_means_no_restriction() {
        // The *other* real-world shape mentioned: the bare "0" default
        // seen in a real XiboClient.config dump (a different, local
        // representation from the live XMDS wire format, but still
        // worth tolerating gracefully rather than erroring/misbehaving
        // if it were ever sent this way too).
        assert_eq!(PlayerSettings::parse_hhmm("0"), None,
                    "a bare integer has no ':' separator to split on");
        let s = PlayerSettings {
            download_start_window: "0".into(),
            download_end_window: "0".into(),
            ..serde_json::from_str("{}").unwrap()
        };
        assert!(s.is_within_download_window(), "bare \"0\" on both sides must fail open");
    }

    #[test]
    fn mixed_colon_and_real_value_still_fails_open() {
        // Asymmetric/partial configurations (one side set, the other
        // still the "unset" placeholder) should also fail open rather
        // than doing something undefined -- `is_within_download_window`
        // requires *both* sides to parse successfully.
        let s = PlayerSettings {
            download_start_window: ":".into(),
            download_end_window: "15:00".into(),
            ..serde_json::from_str("{}").unwrap()
        };
        assert!(s.is_within_download_window());
    }
}

#[cfg(test)]
mod log_level_tests {
    use super::*;

    fn settings_with_log_level(level: &str) -> PlayerSettings {
        PlayerSettings {
            log_level: level.into(),
            ..serde_json::from_str("{}").unwrap()
        }
    }

    #[test]
    fn maps_all_known_cms_values_correctly() {
        assert_eq!(settings_with_log_level("error").log_level_filter(), log::LevelFilter::Error);
        assert_eq!(settings_with_log_level("warn").log_level_filter(), log::LevelFilter::Warn);
        assert_eq!(settings_with_log_level("warning").log_level_filter(), log::LevelFilter::Warn);
        assert_eq!(settings_with_log_level("info").log_level_filter(), log::LevelFilter::Info);
        assert_eq!(settings_with_log_level("audit").log_level_filter(), log::LevelFilter::Info);
        assert_eq!(settings_with_log_level("debug").log_level_filter(), log::LevelFilter::Debug);
        assert_eq!(settings_with_log_level("trace").log_level_filter(), log::LevelFilter::Trace);
    }

    #[test]
    fn is_case_insensitive() {
        assert_eq!(settings_with_log_level("ERROR").log_level_filter(), log::LevelFilter::Error);
        assert_eq!(settings_with_log_level("Debug").log_level_filter(), log::LevelFilter::Debug);
    }

    #[test]
    fn unrecognized_value_falls_back_to_info_not_silently_ignored() {
        assert_eq!(settings_with_log_level("something-unexpected").log_level_filter(),
                   log::LevelFilter::Info);
        assert_eq!(settings_with_log_level("").log_level_filter(), log::LevelFilter::Info);
    }

    #[test]
    fn make_rustls_client_config_is_safe_to_call_more_than_once() {
        // Regression test for a real crash report: "Player now freeze
        // on splash screen", panicking inside this function. Calling it
        // a second time (matching what genuinely happens via the
        // --allow-offline XMR retry path in mainloop.rs, not just a
        // theoretical scenario) must not panic -- the crypto provider
        // can only be *installed* once per process, but that's fine;
        // a second, redundant install attempt should be a silent no-op,
        // not fatal.
        let cms = CmsSettings {
            address: "https://example.com".into(),
            key: "testkey".into(),
            display_id: "test-display".into(),
            display_name: None,
            proxy: None,
        };
        // First call -- matches the normal startup path.
        cms.make_rustls_client_config(false).unwrap();
        // Second call -- matches a later XMR retry re-creating its own
        // TLS config from scratch. Must not panic.
        cms.make_rustls_client_config(false).unwrap();
        // A third, for good measure, with the other no_verify branch.
        cms.make_rustls_client_config(true).unwrap();
    }
}
