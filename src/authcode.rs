// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! "Register via Code" (the Windows/Android player's "Use Code" button):
//! shows a short user-facing code, which an administrator enters into
//! the CMS's own "Add Display (Code)" page, resolving to a real CMS
//! address + key without anyone needing to type either by hand on the
//! display itself.
//!
//! CONFIRMED, not guessed: this whole exchange goes through a *third-party*
//! service operated by Xibo Signage Ltd (`auth.signlicence.co.uk`), NOT
//! the self-hosted CMS directly -- read in full from the real, current
//! source of the official Windows .NET player (OptionsForm.xaml.cs,
//! xibosignage/xibo-dotnetclient, `develop` branch, fetched during
//! development). Requires outbound internet access to that specific host;
//! a CMS reachable only over a private/restricted network (no route to
//! the public internet) cannot use this feature at all, regardless of
//! anything arexibo itself does.
//!
//! Protocol (an OAuth2 Device Authorization Grant shape, though nothing
//! here is standard OAuth):
//! 1. `POST https://auth.signlicence.co.uk/generateCode` with
//!    `{"hardwareId", "type", "version"}` -> `{"user_code", "device_code"}`
//!    (`user_code`: the 6-character code to show; `device_code`: kept
//!    secret client-side, never displayed).
//! 2. Poll (official client: every 10s) `GET https://auth.signlicence.co
//!    .uk/getDetails?user_code=...&device_code=...` until the response
//!    contains a `cmsAddress` key -- at that point it also contains
//!    `cmsKey`, and registration proceeds exactly as if those had been
//!    typed in manually (this module's own job ends there; seeting up
//!    the real Cms/Handler is main.rs's job, same as the manual path).

use std::time::Duration;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;

const AUTH_HOST: &str = "https://auth.signlicence.co.uk";

/// Matches the official Windows client's own `type` field for this
/// exchange -- confirmed as a literal `"windows"` there (not e.g.
/// "linux"), and nothing in the exchange's own confirmed shape suggests
/// the CMS-side behavior depends on this value beyond display purposes,
/// so no separate arexibo-specific value was invented.
const CLIENT_TYPE: &str = "windows";

pub struct GeneratedCode {
    /// The short code to display to a human, who enters it into the
    /// CMS's own "Add Display (Code)" page.
    pub user_code: String,
    /// Kept secret, never displayed -- proves to the auth service that
    /// whoever polls `check_code` is the same device that generated
    /// this code in the first place.
    pub device_code: String,
}

pub struct ClaimedCode {
    pub cms_address: String,
    pub cms_key: String,
}

fn make_agent(proxy: Option<&str>) -> Result<ureq::Agent> {
    let proxy = match proxy {
        Some(p) => Some(ureq::Proxy::new(p)?),
        None => None,
    };
    Ok(ureq::config::Config::builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(Duration::from_secs(15)))
        .proxy(proxy)
        .build().into())
}

#[derive(Deserialize)]
struct GenerateCodeResponse {
    user_code: Option<String>,
    device_code: Option<String>,
    // Confirmed from the real client: a `message` key in the response
    // means something went wrong server-side even on an HTTP 2xx --
    // treated as a failure rather than trusting user_code/device_code
    // to be meaningfully present.
    message: Option<String>,
}

pub fn generate_code(hardware_id: &str, client_version: &str, proxy: Option<&str>) -> Result<GeneratedCode> {
    let agent = make_agent(proxy)?;
    let body = serde_json::to_vec(&json!({
        "hardwareId": hardware_id,
        "type": CLIENT_TYPE,
        "version": client_version,
    }))?;
    let resp = agent.post(&format!("{AUTH_HOST}/generateCode"))
        .header("Content-Type", "application/json")
        .send(&body)
        .context("requesting a registration code")?;
    let text = resp.into_body().read_to_string().context("reading generateCode response")?;
    let parsed: GenerateCodeResponse = serde_json::from_str(&text)
        .with_context(|| format!("parsing generateCode response: {text:?}"))?;
    if let Some(msg) = parsed.message {
        bail!("auth service returned an error: {msg}");
    }
    let user_code = parsed.user_code.context("generateCode response missing user_code")?;
    let device_code = parsed.device_code.context("generateCode response missing device_code")?;
    Ok(GeneratedCode { user_code, device_code })
}

#[derive(Deserialize)]
struct GetDetailsResponse {
    #[serde(rename = "cmsAddress")]
    cms_address: Option<String>,
    #[serde(rename = "cmsKey")]
    cms_key: Option<String>,
}

/// One poll attempt. `Ok(None)` means "not claimed yet, keep polling" --
/// NOT an error, this is the expected result of nearly every call until
/// an administrator actually enters the code in the CMS.
pub fn check_code(user_code: &str, device_code: &str, proxy: Option<&str>) -> Result<Option<ClaimedCode>> {
    let agent = make_agent(proxy)?;
    let url = format!("{AUTH_HOST}/getDetails?user_code={user_code}&device_code={device_code}");
    let resp = agent.get(&url).call()
        .with_context(|| "checking registration code".to_string())?;
    let text = resp.into_body().read_to_string().context("reading getDetails response")?;
    let parsed: GetDetailsResponse = serde_json::from_str(&text)
        .with_context(|| format!("parsing getDetails response: {text:?}"))?;
    match (parsed.cms_address, parsed.cms_key) {
        (Some(cms_address), Some(cms_key)) => Ok(Some(ClaimedCode { cms_address, cms_key })),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These exercise only the local parsing/decision logic against
    // canned JSON -- no real network call to the actual third-party
    // service (which would make tests flaky/slow and hit someone else's
    // production service from CI).

    #[test]
    fn generate_code_response_message_is_an_error_even_with_other_fields_present() {
        let raw = r#"{"message": "rate limited", "user_code": "ABC123", "device_code": "secret"}"#;
        let parsed: GenerateCodeResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.message.as_deref(), Some("rate limited"));
    }

    #[test]
    fn get_details_without_cms_address_is_not_yet_claimed() {
        let raw = r#"{}"#;
        let parsed: GetDetailsResponse = serde_json::from_str(raw).unwrap();
        assert!(parsed.cms_address.is_none());
    }

    #[test]
    fn get_details_with_both_fields_is_claimed() {
        let raw = r#"{"cmsAddress": "https://cms.example.com", "cmsKey": "thekey"}"#;
        let parsed: GetDetailsResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.cms_address.as_deref(), Some("https://cms.example.com"));
        assert_eq!(parsed.cms_key.as_deref(), Some("thekey"));
    }
}
