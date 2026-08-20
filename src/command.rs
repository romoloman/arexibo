// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Player command handling.

use std::{collections::HashMap, io::{Read, Write}, time::Duration, process};
use anyhow::{bail, Context, Result};
use itertools::Itertools;
use serde::{Serialize, Deserialize};

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Command {
    pub command: String,
    pub validate: String,
    pub alerts: String,
}

impl Command {
    pub fn run(&self) -> Result<bool> {
        log::info!("running command {:?}", self.command);
        let result = if self.command == "SoftRestart" {
            std::process::exit(0);
        } else if self.command.starts_with("http|") {
            self.run_http()?
        } else if self.command.starts_with("rs232|") {
            self.run_rs232()?
        } else {
            self.run_shell()?
        };

        if self.validate.is_empty() {
            Ok(true)
        } else {
            log::info!("validating command result {result:?} against {:?}", self.validate);
            let rx = regex::Regex::new(&self.validate).context("invalid validation Regex")?;
            Ok(rx.is_match(&result))
        }
    }

    fn run_shell(&self) -> Result<String> {
        let cmd = process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&self.command)
            .output()
            .context("running shell command")?;
        Ok(String::from_utf8_lossy(&cmd.stdout).into())
    }

    fn run_http(&self) -> Result<String> {
        let (_, url, content_type, opts) = self.command.split('|').collect_tuple()
            .context("invalid HTTP command string")?;
        let opts: HttpOpts = serde_json::from_str(opts)
            .context("invalid HTTP option dictionary")?;

        let mut builder = ureq::http::Request::builder()
            .method(opts.method.as_str())
            .uri(url)
            .header("Content-Type", content_type);
        // Tolerant: an empty/whitespace-only or unparseable headers
        // string (the common real-world case, literally "{}" as text)
        // just means "no extra headers" rather than failing the whole
        // request -- see HttpOpts::headers's own doc comment for why
        // this needs a second parse step at all.
        if !opts.headers.trim().is_empty() {
            match serde_json::from_str::<HashMap<String, String>>(&opts.headers) {
                Ok(headers) => for (k, v) in headers {
                    builder = builder.header(k, v);
                },
                Err(e) => log::warn!("ignoring unparseable HTTP command headers {:?}: {e:#}",
                                      opts.headers),
            }
        }
        let request = builder.body(opts.body).context("invalid HTTP request")?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .build().into();
        let result = agent.run(request).context("making HTTP request")?;

        // strange, but the status code is the only thing used for validation
        Ok(result.status().as_str().into())
    }

    fn run_rs232(&self) -> Result<String> {
        let (_, params, msg) = self.command.split('|').collect_tuple()
            .context("invalid RS232 command string")?;
        let (dev, baud, bits, parity, stop, handshake, hex) =
            params.split(',').collect_tuple().context("invalid RS232 param string")?;
        let baud = baud.parse().context("invalid RS232 baud rate")?;
        let bits = match bits {
            "5" => serialport::DataBits::Five,
            "6" => serialport::DataBits::Six,
            "7" => serialport::DataBits::Seven,
            "8" => serialport::DataBits::Eight,
            _ => bail!("invalid RS232 data bits")
        };
        let parity = match parity {
            "None" => serialport::Parity::None,
            "Odd" => serialport::Parity::Odd,
            "Even" => serialport::Parity::Even,
            _ => bail!("invalid RS232 parity")
        };
        let stop = match stop {
            "None" | "One" => serialport::StopBits::One,
            "OnePointFive" | "Two" => serialport::StopBits::Two,
            _ => bail!("invalid RS232 stop bits")
        };
        let handshake = match handshake {
            "None" => serialport::FlowControl::None,
            "XOnXOff" => serialport::FlowControl::Software,
            "RequestToSend" => serialport::FlowControl::Hardware,
            _ => bail!("invalid RS232 handshake")
        };

        let mut port = serialport::new(dev, baud)
            .data_bits(bits)
            .stop_bits(stop)
            .parity(parity)
            .flow_control(handshake)
            .timeout(TIMEOUT)
            .open_native()?;

        let data = if hex == "1" {
            let msg = msg.chars().filter(|&c| !c.is_whitespace()).collect::<String>();
            hex::decode(msg).context("invalid RS232 hex message")?
        } else {
            msg.as_bytes().to_vec()
        };

        port.write(&data).context("writing RS232 message")?;

        if self.validate.is_empty() {
            // don't try to read if it's not used
            return Ok(String::new());
        }

        let mut buf = [0];
        let mut result = String::new();
        loop {
            port.read_exact(&mut buf).context("reading RS232 result")?;
            if buf[0] == b'\n' {
                break;
            }
            result.push(buf[0] as char);
        }
        Ok(result)
    }
}

#[derive(Deserialize)]
struct HttpOpts {
    method: String,
    // NOT a nested JSON object -- the real payload has
    // "headers":"{}", a STRING whose content happens to be JSON text
    // (same as `body` below). Parsed as a plain string here, then
    // parsed again as JSON where actually used (see run_http).
    headers: String,
    body: String
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_opts_parses_real_payload_from_user_log() {
        // Regression test using the exact JSON the user's log showed
        // failing: "invalid type: string \"{}\", expected a map".
        let json = r#"{"method":"GET","headers":"{}","body":"{}"}"#;
        let opts: HttpOpts = serde_json::from_str(json).unwrap();
        assert_eq!(opts.method, "GET");
        assert_eq!(opts.headers, "{}");
        assert_eq!(opts.body, "{}");
    }

    #[test]
    fn http_opts_parses_populated_headers_string() {
        let json = r#"{"method":"POST","headers":"{\"Authorization\":\"Bearer xyz\"}","body":"{}"}"#;
        let opts: HttpOpts = serde_json::from_str(json).unwrap();
        let headers: HashMap<String, String> = serde_json::from_str(&opts.headers).unwrap();
        assert_eq!(headers.get("Authorization"), Some(&"Bearer xyz".to_string()));
    }

    #[test]
    fn empty_headers_string_parses_to_empty_map() {
        let headers: HashMap<String, String> = serde_json::from_str("{}").unwrap();
        assert!(headers.is_empty());
    }
}
