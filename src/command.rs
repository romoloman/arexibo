// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Player command handling.

use std::{collections::{BTreeMap, HashMap}, io::{Read, Write}, time::Duration, process};
use anyhow::{bail, Context, Result};
use itertools::Itertools;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Serialize, Deserialize};

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Command {
    pub command: String,
    pub validate: String,
    pub alerts: String,
}

impl Command {
    /// Runs the command, with its `[[tag]]` placeholders replaced by the
    /// display's tags (see `resolve_segment`).
    pub fn run(&self, tags: &BTreeMap<String, String>) -> Result<bool> {
        log::info!("running command {:?}", self.command);
        let result = if self.command == "SoftRestart" {
            std::process::exit(0);
        } else if self.command.starts_with("http|") {
            self.run_http(&self.segments(tags)?)?
        } else if self.command.starts_with("rs232|") {
            self.run_rs232(&self.segments(tags)?)?
        } else {
            self.run_shell(&resolve_shell(&self.command, tags)?)?
        };

        if self.validate.is_empty() {
            Ok(true)
        } else {
            log::info!("validating command result {result:?} against {:?}", self.validate);
            let rx = regex::Regex::new(&self.validate).context("invalid validation Regex")?;
            Ok(rx.is_match(&result))
        }
    }

    /// The `|`-separated parts, each with its placeholders resolved on its own,
    /// so a value containing `|` cannot move a boundary.
    fn segments(&self, tags: &BTreeMap<String, String>) -> Result<Vec<String>> {
        self.command.split('|').map(|segment| resolve_segment(segment, tags)).collect()
    }

    fn run_shell(&self, command: &str) -> Result<String> {
        let cmd = process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .output()
            .context("running shell command")?;
        Ok(String::from_utf8_lossy(&cmd.stdout).into())
    }

    fn run_http(&self, segments: &[String]) -> Result<String> {
        let (_, url, content_type, opts) = segments.iter().map(String::as_str).collect_tuple()
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
            .tls_config(crate::tls::http_config(false))
            .build().into();
        let result = agent.run(request).context("making HTTP request")?;

        // strange, but the status code is the only thing used for validation
        Ok(result.status().as_str().into())
    }

    fn run_rs232(&self, segments: &[String]) -> Result<String> {
        let (_, params, msg) = segments.iter().map(String::as_str).collect_tuple()
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

static PLACEHOLDER: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[\[([A-Za-z0-9_]+)\]\]").unwrap());
static URL_SEGMENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.\-]*://").unwrap());
static JSON_SEGMENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*[{\[]").unwrap());
static LEADING_PLACEHOLDER: Lazy<Regex> = Lazy::new(|| Regex::new(
    r"^\s*(?:\[\[[A-Za-z0-9_]+\]\]|\[[A-Z_]+(?:[+-]\d+|@(?:[01]\d|2[0-3]):[0-5]\d)?\])").unwrap());

#[derive(Clone, Copy)]
enum Escape {
    None,
    Url,
    Json,
}

/// As the Electron player: by the shape of the segment, so a value typed by
/// hand cannot cut a URL short or break a JSON document.
fn escape_for(segment: &str) -> Escape {
    if URL_SEGMENT.is_match(segment) {
        Escape::Url
    } else if JSON_SEGMENT.is_match(segment) && !LEADING_PLACEHOLDER.is_match(segment) {
        Escape::Json
    } else {
        Escape::None
    }
}

/// JavaScript's `encodeURIComponent`.
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// How many JSON strings enclose the placeholder at `offset`: a value in an
/// HTTP command's body sits two deep (a JSON string inside the option
/// dictionary), and each level needs a round of escaping.
fn json_depth_at(segment: &str, offset: usize) -> usize {
    let preceding = &segment[..offset];
    let Some(quote) = preceding.rfind('"') else { return 1 };
    let backslashes = preceding[..quote].bytes().rev().take_while(|&b| b == b'\\').count();
    ((backslashes + 1) as f64).log2().round() as usize + 1
}

fn json_escape(value: &str, depth: usize) -> String {
    let mut escaped = value.to_string();
    for _ in 0..depth {
        let quoted = serde_json::to_string(&escaped).expect("a string as json");
        escaped = quoted[1..quoted.len() - 1].to_string();
    }
    escaped
}

fn resolve(segment: &str, tags: &BTreeMap<String, String>, escape: Escape) -> Result<String> {
    let mut out = String::with_capacity(segment.len());
    let mut last = 0;
    for caps in PLACEHOLDER.captures_iter(segment) {
        let whole = caps.get(0).expect("whole match");
        let name = &caps[1];
        let value = match tags.get(name) {
            None => bail!("unresolvable tag [[{name}]]: not set on this display"),
            Some(v) if v.is_empty() => bail!("unresolvable tag [[{name}]]: set, but has no value"),
            Some(v) => v,
        };
        out.push_str(&segment[last..whole.start()]);
        out.push_str(&match escape {
            Escape::None => value.clone(),
            Escape::Url => encode_uri_component(value),
            Escape::Json => json_escape(value, json_depth_at(segment, whole.start())),
        });
        last = whole.end();
    }
    out.push_str(&segment[last..]);
    Ok(out)
}

/// Replaces each `[[name]]` in one part of a command string with the
/// display's tag of that name (matched exactly), escaped for the shape of the
/// part. A tag that is missing or empty is an error: the command must not
/// run with a value the device would act on nonsensically. A value is
/// substituted once, never scanned again.
pub fn resolve_segment(segment: &str, tags: &BTreeMap<String, String>) -> Result<String> {
    resolve(segment, tags, escape_for(segment))
}

/// The same for a shell command line, which is one segment and takes the
/// value as typed: quote the placeholder in the command where needed.
pub fn resolve_shell(command: &str, tags: &BTreeMap<String, String>) -> Result<String> {
    resolve(command, tags, Escape::None)
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
mod tag_tests {
    use super::*;

    fn tags(list: &[(&str, &str)]) -> BTreeMap<String, String> {
        list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn resolved(segment: &str, list: &[(&str, &str)]) -> String {
        resolve_segment(segment, &tags(list)).unwrap()
    }

    #[test]
    fn a_plain_segment_takes_the_value_as_typed() {
        assert_eq!(resolved("echo [[city]] [[city]]", &[("city", "Massa & Carrara")]),
                   "echo Massa & Carrara Massa & Carrara");
        assert_eq!(resolved("[[a]]", &[("a", "x")]), "x");
        assert_eq!(resolved("no placeholders", &[]), "no placeholders");
    }

    #[test]
    fn a_url_segment_encodes_values_like_encode_uri_component() {
        assert_eq!(resolved("http://h/api?c=[[city]]&n=[[n]]", &[("city", "Massa Carrara & co"), ("n", "2")]),
                   "http://h/api?c=Massa%20Carrara%20%26%20co&n=2");
        assert_eq!(resolved("https://h/[[v]]", &[("v", "a/b+c?d=e#f")]), "https://h/a%2Fb%2Bc%3Fd%3De%23f");
        assert_eq!(resolved("http://h/[[v]]", &[("v", "caff\u{e8} \u{1F600}")]), "http://h/caff%C3%A8%20%F0%9F%98%80");
        assert_eq!(resolved("http://h/[[v]]", &[("v", "-_.!~*'()AZaz09")]), "http://h/-_.!~*'()AZaz09",
                   "the characters encodeURIComponent leaves alone");
    }

    #[test]
    fn a_json_segment_escapes_values_for_the_string_they_sit_in() {
        assert_eq!(resolved(r#"{"a":"[[x]]"}"#, &[("x", r#"He said "yes" \ ok"#)]),
                   r#"{"a":"He said \"yes\" \\ ok"}"#);
        assert_eq!(resolved(r#"["[[x]]"]"#, &[("x", "a\nb")]), r#"["a\nb"]"#);
        let out = resolved(r#"{"a":"[[x]]"}"#, &[("x", "caff\u{e8}")]);
        assert_eq!(out, "{\"a\":\"caff\u{e8}\"}", "non-ASCII is left as it is");
    }

    #[test]
    fn a_value_in_an_http_commands_body_is_escaped_two_levels_deep() {
        // The option dictionary is JSON whose `body` is a string holding JSON.
        let segment = r#"{"method":"POST","headers":"{}","body":"{\"c\":\"[[x]]\"}"}"#;
        for value in ["plain", r#"a"b"#, r"a\b", "line\nbreak", r#"{"nested": "q\"uote"}"#] {
            let out = resolved(segment, &[("x", value)]);
            let opts: serde_json::Value = serde_json::from_str(&out).expect("the dictionary stays valid JSON");
            let body: serde_json::Value = serde_json::from_str(opts["body"].as_str().unwrap())
                .expect("and so does the body inside it");
            assert_eq!(body["c"], value, "{value:?}");
        }
        // one level for a value in the dictionary itself
        let out = resolved(r#"{"method":"[[m]]","headers":"{}","body":"{}"}"#, &[("m", r#"a"b"#)]);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["method"], r#"a"b"#);
    }

    #[test]
    fn a_segment_opening_with_a_placeholder_is_not_taken_for_json() {
        assert_eq!(resolved(r#"[[a]] "quoted""#, &[("a", r#"x"y"#)]), r#"x"y "quoted""#);
        assert_eq!(resolved("[[a]]", &[("a", r#"x"y"#)]), r#"x"y"#);
        assert_eq!(resolved(r#"[EPOCH] "[[a]]""#, &[("a", r#"x"y"#)]), r#"[EPOCH] "x"y""#);
    }

    #[test]
    fn a_missing_or_empty_tag_is_an_error_naming_it() {
        let err = resolve_segment("echo [[city]]", &tags(&[])).unwrap_err().to_string();
        assert!(err.contains("[[city]]") && err.contains("not set"), "{err}");
        let err = resolve_segment("echo [[city]]", &tags(&[("city", "")])).unwrap_err().to_string();
        assert!(err.contains("[[city]]") && err.contains("no value"), "{err}");
        assert!(resolve_segment("[[a]] [[b]]", &tags(&[("a", "1")])).is_err(), "any one missing fails the whole");
    }

    #[test]
    fn tag_names_are_matched_exactly() {
        assert!(resolve_segment("[[City]]", &tags(&[("city", "x")])).is_err());
        assert!(resolve_segment("[[city ]]", &tags(&[("city", "x")])).is_ok(), "not a placeholder: left alone");
    }

    #[test]
    fn text_that_is_not_a_placeholder_is_left_alone() {
        let list = [("a", "x")];
        for text in ["[a]", "[epoch]", "[[ a ]]", "[[a]", "[ -f /tmp/x ] && echo ok", "[[ -f /tmp/x ]]", "[[]]", "[[a-b]]"] {
            assert_eq!(resolved(text, &list), text);
        }
    }

    #[test]
    fn a_substituted_value_is_not_scanned_again() {
        assert_eq!(resolved("[[a]]", &[("a", "[[b]]"), ("b", "x")]), "[[b]]");
    }

    #[test]
    fn a_value_with_a_bar_cannot_move_a_segment_boundary() {
        let cmd = Command { command: "rs232|/dev/ttyS0,9600,8,None,One,None,0|[[msg]]".into(),
                            validate: String::new(), alerts: String::new() };
        let segments = cmd.segments(&tags(&[("msg", "a|b|c")])).unwrap();
        assert_eq!(segments, ["rs232", "/dev/ttyS0,9600,8,None,One,None,0", "a|b|c"]);
    }

    #[test]
    fn a_shell_command_is_one_segment_taking_the_value_as_typed() {
        let t = tags(&[("name", "it's")]);
        assert_eq!(resolve_shell(r#"echo "[[name]]" | tr a-z A-Z"#, &t).unwrap(), r#"echo "it's" | tr a-z A-Z"#);
        assert_eq!(resolve_shell("[ -n '[[name]]' ] && echo ok", &t).unwrap(), "[ -n 'it's' ] && echo ok",
                   "even when it opens like a JSON array: a shell line is never JSON-escaped");
    }

    #[test]
    fn a_shell_command_runs_with_the_tag() {
        let cmd = Command { command: "printf '%s' '[[city]]'".into(), validate: "^Massa Carrara$".into(), alerts: String::new() };
        assert!(cmd.run(&tags(&[("city", "Massa Carrara")])).unwrap());
        assert!(!cmd.run(&tags(&[("city", "Pisa")])).unwrap(), "the validation sees the substituted value");
    }

    #[test]
    fn a_command_with_an_unresolvable_tag_does_not_run() {
        let marker = std::env::temp_dir().join(format!("arexibo_cmd_tag_{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let cmd = Command { command: format!("touch {} && echo [[missing]]", marker.display()),
                            validate: String::new(), alerts: String::new() };
        assert!(cmd.run(&tags(&[])).is_err());
        assert!(!marker.exists(), "nothing was executed");
    }

    #[test]
    fn a_command_without_placeholders_is_unchanged() {
        let cmd = Command { command: "printf '%s' '[ok]'".into(), validate: "^\\[ok\\]$".into(), alerts: String::new() };
        assert!(cmd.run(&tags(&[])).unwrap());
    }

    /// A mock HTTP server recording the first request: method, url, content type, body.
    fn serve_once() -> (u16, std::sync::mpsc::Receiver<(String, String, String, String)>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(mut req) = server.recv() {
                let mut body = String::new();
                let _ = req.as_reader().read_to_string(&mut body);
                let ct = req.headers().iter().find(|h| h.field.equiv("Content-Type"))
                    .map(|h| h.value.to_string()).unwrap_or_default();
                let _ = tx.send((req.method().to_string(), req.url().to_string(), ct, body));
                let _ = req.respond(tiny_http::Response::from_string("ok"));
            }
        });
        (port, rx)
    }

    #[test]
    fn an_http_command_sends_the_tags_in_the_url_and_the_body() {
        let (port, rx) = serve_once();
        let command = format!(
            r#"http|http://127.0.0.1:{port}/api/[[city]]?q=[[q]]|application/json|{{"method":"POST","headers":"{{}}","body":"{{\"who\":\"[[q]]\"}}"}}"#);
        let cmd = Command { command, validate: "^200$".into(), alerts: String::new() };
        assert!(cmd.run(&tags(&[("city", "Massa Carrara"), ("q", r#"a "b" & c"#)])).unwrap());
        let (method, url, content_type, body) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(url, "/api/Massa%20Carrara?q=a%20%22b%22%20%26%20c");
        assert_eq!(content_type, "application/json");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["who"], r#"a "b" & c"#);
    }

    #[test]
    fn an_http_command_with_an_unresolvable_tag_sends_nothing() {
        let (port, rx) = serve_once();
        let command = format!(r#"http|http://127.0.0.1:{port}/[[city]]|text/plain|{{"method":"GET","headers":"{{}}","body":"{{}}"}}"#);
        let cmd = Command { command, validate: String::new(), alerts: String::new() };
        assert!(cmd.run(&tags(&[])).is_err());
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "no request was made");
    }
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
