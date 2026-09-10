// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! "Register via Code" (Windows/Android player's "Use Code" button).
//! Goes through a third-party service (`auth.signlicence.co.uk`), not
//! the self-hosted CMS directly -- requires outbound internet access
//! to that host specifically. Protocol: `generate_code` gets a
//! user-facing code + secret device code; `check_code` polls until
//! claimed, returning a real CMS address/key.

use std::time::Duration;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;

const AUTH_HOST: &str = "https://auth.signlicence.co.uk";

/// Matches the official Windows client's own `type` field.
const CLIENT_TYPE: &str = "windows";

pub struct GeneratedCode {
    /// Shown on screen for an admin to enter in the CMS.
    pub user_code: String,
    /// Kept secret, never displayed.
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
    // A `message` key means an error even on HTTP 2xx.
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

/// `Ok(None)` means not yet claimed -- not an error.
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

    // Only local parsing/decision logic -- no real network call.

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
