//! The `operatingSystem` of the registration, as the Windows player sends it
//! (`HardwareKey.OperatingSystemAsJson`): a JSON with the manufacturer, the
//! brand, the model, the OS version and the "sdk", which the CMS reads into
//! the display's details (`Soap5::RegisterDisplay`; anything that is not
//! JSON goes whole into the OS version). On Linux: the names come from the
//! firmware's DMI data, the OS from `/etc/os-release` and the kernel release.

use std::path::Path;
use std::sync::OnceLock;
use serde::Serialize;

/// The OS version when there is nothing to read it from.
const FALLBACK_VERSION: &str = if cfg!(windows) { "windows" } else { "linux" };

/// What firmware leaves where a real name should be (the Windows player's list).
const GENERIC: [&str; 5] =
    ["System Product Name", "To Be Filled By O.E.M.", "Default string", "Not Applicable", "System Version"];

fn is_generic(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    // "O.E.M." or "OEM" alone is a placeholder too, but a name that has them
    // in it is a name.
    value.is_empty() || value == "o.e.m." || value == "oem"
        || GENERIC.iter().any(|placeholder| value.contains(&placeholder.to_ascii_lowercase()))
}

#[derive(Debug, Default, PartialEq, Serialize)]
struct OperatingSystem {
    manufacturer: String,
    brand: String,
    model: String,
    version: String,
    sdk: String,
}

/// The value of an os-release line, without its quotes (and the escapes the
/// format allows inside double quotes).
fn unquote(value: &str) -> String {
    let value = value.trim();
    if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match (c, chars.clone().next()) {
                ('\\', Some(next @ ('"' | '\\' | '$' | '`'))) => {
                    out.push(next);
                    chars.next();
                }
                _ => out.push(c),
            }
        }
        out
    } else if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        inner.to_string()
    } else {
        value.to_string()
    }
}

/// `PRETTY_NAME` of an os-release file, else its `NAME` and `VERSION_ID`.
fn os_name(os_release: &str) -> Option<String> {
    let value = |key: &str| {
        os_release.lines()
            .find_map(|line| {
                let (name, value) = line.split_once('=')?;
                (name.trim() == key).then(|| unquote(value))
            })
            .filter(|value| !value.is_empty())
    };
    value("PRETTY_NAME").or_else(|| {
        let name = value("NAME")?;
        Some(match value("VERSION_ID") {
            Some(version) => format!("{name} {version}"),
            None => name,
        })
    })
}

/// A file's text, trimmed of whitespace and of the NULs of the device tree.
fn read_trimmed(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    (!text.is_empty()).then(|| text.to_string())
}

fn build(os_release: Option<&str>, kernel: Option<&str>, dmi: impl Fn(&str) -> Option<String>,
         device_tree_model: Option<&str>) -> OperatingSystem {
    let real = |name: &str| dmi(name).filter(|value| !is_generic(value));
    let manufacturer = real("sys_vendor").or_else(|| real("board_vendor")).unwrap_or_default();
    let brand = real("sys_vendor").unwrap_or_else(|| manufacturer.clone());
    let model = real("product_name")
        .or_else(|| real("board_name"))
        .or_else(|| device_tree_model.map(str::trim).filter(|m| !is_generic(m)).map(str::to_string))
        .unwrap_or_default();
    OperatingSystem {
        manufacturer, brand, model,
        version: os_release.and_then(os_name).unwrap_or_else(|| FALLBACK_VERSION.to_string()),
        sdk: kernel.map(str::trim).unwrap_or_default().to_string(),
    }
}

fn system() -> OperatingSystem {
    let os_release = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release")).ok();
    let kernel = read_trimmed(Path::new("/proc/sys/kernel/osrelease"));
    let device_tree_model = read_trimmed(Path::new("/sys/firmware/devicetree/base/model"));
    build(os_release.as_deref(), kernel.as_deref(),
          |name| read_trimmed(&Path::new("/sys/class/dmi/id").join(name)), device_tree_model.as_deref())
}

/// The generated SOAP code writes the values as they are, without escaping.
fn soap_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The JSON, as it is.
pub fn json() -> &'static str {
    static JSON: OnceLock<String> = OnceLock::new();
    JSON.get_or_init(|| serde_json::to_string(&system()).expect("serializing the operating system"))
}

/// The JSON, for the text of a SOAP request.
pub fn json_for_soap() -> &'static str {
    static JSON: OnceLock<String> = OnceLock::new();
    JSON.get_or_init(|| soap_escape(json()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn dmi<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string())
    }

    fn json_of(os: &OperatingSystem) -> String {
        serde_json::to_string(os).unwrap()
    }

    #[test]
    fn the_pretty_name_is_the_os_name() {
        let ubuntu = "NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nPRETTY_NAME=\"Ubuntu 24.04.4 LTS\"\nID=ubuntu\n";
        assert_eq!(os_name(ubuntu).as_deref(), Some("Ubuntu 24.04.4 LTS"));
        assert_eq!(os_name("PRETTY_NAME='Debian GNU/Linux 13 (trixie)'\n").as_deref(), Some("Debian GNU/Linux 13 (trixie)"));
        assert_eq!(os_name("PRETTY_NAME=Arch\n").as_deref(), Some("Arch"));
        assert_eq!(os_name("# a comment\nID_LIKE=debian\nPRETTY_NAME = \"Spaced\"\n").as_deref(), Some("Spaced"));
    }

    #[test]
    fn without_a_pretty_name_it_is_the_name_and_the_version() {
        assert_eq!(os_name("NAME=\"Foo OS\"\nVERSION_ID=\"3.1\"\n").as_deref(), Some("Foo OS 3.1"));
        assert_eq!(os_name("NAME=\"Foo OS\"\n").as_deref(), Some("Foo OS"));
        assert_eq!(os_name("PRETTY_NAME=\"\"\nNAME=\"Foo\"\n").as_deref(), Some("Foo"), "an empty one is not a name");
    }

    #[test]
    fn nothing_usable_is_none() {
        for text in ["", "ID=foo\n", "garbage without equals", "VERSION_ID=\"1\"\n"] {
            assert_eq!(os_name(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_escapes_inside_double_quotes_are_undone() {
        assert_eq!(os_name("PRETTY_NAME=\"Say \\\"hi\\\" \\\\ ok\"\n").as_deref(), Some("Say \"hi\" \\ ok"));
        assert_eq!(os_name("PRETTY_NAME='keeps \\n as is'\n").as_deref(), Some("keeps \\n as is"));
    }

    #[test]
    fn firmware_placeholders_are_not_names() {
        for placeholder in ["System Product Name", "To Be Filled By O.E.M.", "to be filled by o.e.m.", "Default string",
                            "Not Applicable", "System Version", "  Default string  ", "", "   ",
                            "xx To Be Filled By O.E.M. xx", "O.E.M.", "o.e.m.", "OEM", " oem "] {
            assert!(is_generic(placeholder), "{placeholder:?}");
        }
        for name in ["PP7600 A", "Dell Inc.", "ThinkCentre M920q", "Raspberry Pi 4 Model B Rev 1.4", "Dell OEM Parts", "OEM Industrial Co", "O.E.M. Systems"] {
            assert!(!is_generic(name), "{name}");
        }
    }

    #[test]
    fn a_pc_with_a_real_firmware_is_described_by_it() {
        let pairs = [("sys_vendor", "ACME Corp"), ("product_name", "PP7600 A"), ("board_vendor", "Board Co"), ("board_name", "B1")];
        let os = build(Some("PRETTY_NAME=\"Ubuntu 26.04 LTS\"\n"), Some("6.17.0-5-generic\n"), dmi(&pairs), None);
        assert_eq!(json_of(&os), "{\"manufacturer\":\"ACME Corp\",\"brand\":\"ACME Corp\",\"model\":\"PP7600 A\",\
                                  \"version\":\"Ubuntu 26.04 LTS\",\"sdk\":\"6.17.0-5-generic\"}");
    }

    #[test]
    fn the_board_stands_in_when_the_firmware_has_placeholders() {
        let pairs = [("sys_vendor", "To Be Filled By O.E.M."), ("product_name", "System Product Name"),
                     ("board_vendor", "Board Co"), ("board_name", "B1")];
        let os = build(None, None, dmi(&pairs), None);
        assert_eq!((os.manufacturer.as_str(), os.brand.as_str(), os.model.as_str()), ("Board Co", "Board Co", "B1"));
    }

    #[test]
    fn a_board_with_nothing_real_has_empty_names() {
        let pairs = [("sys_vendor", "Default string"), ("product_name", "Default string"),
                     ("board_vendor", "Default string"), ("board_name", "Not Applicable")];
        let os = build(None, None, dmi(&pairs), None);
        assert_eq!((os.manufacturer.as_str(), os.brand.as_str(), os.model.as_str()), ("", "", ""));
    }

    #[test]
    fn a_raspberry_pi_has_no_dmi_and_names_itself_in_the_device_tree() {
        let os = build(Some("PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n"), Some("6.12.47+rpt-rpi-v8"), |_| None,
                       Some("Raspberry Pi 4 Model B Rev 1.4"));
        assert_eq!(os.model, "Raspberry Pi 4 Model B Rev 1.4");
        assert_eq!((os.manufacturer.as_str(), os.brand.as_str()), ("", ""));
        assert_eq!(os.version, "Debian GNU/Linux 13 (trixie)");
    }

    #[test]
    fn without_an_os_release_the_version_is_what_it_used_to_be() {
        let os = build(None, None, |_| None, None);
        assert_eq!((os.version.as_str(), os.sdk.as_str()), (FALLBACK_VERSION, ""));
    }

    #[test]
    fn a_name_cannot_break_the_request() {
        let pairs = [("sys_vendor", "A & B <C> \"D\""), ("product_name", "M")];
        let json = json_of(&build(None, None, dmi(&pairs), None));
        let escaped = soap_escape(&json);
        assert!(!escaped.contains('<') && !escaped.contains('>'), "{escaped}");
        assert!(escaped.contains("A &amp; B &lt;C&gt;"), "{escaped}");
        let back = escaped.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
        assert_eq!(back, json, "what the CMS reads back from the XML");
        let value: Value = serde_json::from_str(&back).unwrap();
        assert_eq!(value["manufacturer"], "A & B <C> \"D\"");
    }

    #[test]
    fn a_firmware_that_says_oem_everywhere_leaves_the_names_empty() {
        let pairs = [("sys_vendor", "O.E.M."), ("product_name", "O.E.M."), ("board_vendor", "O.E.M."), ("board_name", "O.E.M.")];
        let os = build(Some("PRETTY_NAME=\"Ubuntu 26.04.1 LTS\"\n"), Some("7.0.0-29-generic"), dmi(&pairs), None);
        assert_eq!(json_of(&os), "{\"manufacturer\":\"\",\"brand\":\"\",\"model\":\"\",\
                                  \"version\":\"Ubuntu 26.04.1 LTS\",\"sdk\":\"7.0.0-29-generic\"}");
    }

    #[test]
    fn a_real_board_name_is_used_when_the_system_says_oem() {
        let pairs = [("sys_vendor", "O.E.M."), ("product_name", "O.E.M."), ("board_vendor", "Board Co"), ("board_name", "EKL-1")];
        let os = build(None, None, dmi(&pairs), None);
        assert_eq!((os.manufacturer.as_str(), os.brand.as_str(), os.model.as_str()), ("Board Co", "Board Co", "EKL-1"));
    }

    #[test]
    fn what_the_device_tree_ends_with_is_trimmed() {
        let dir = std::env::temp_dir().join(format!("arexibo_osinfo_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("model"), "Raspberry Pi 4 Model B Rev 1.4\0\n").unwrap();
        std::fs::write(dir.join("empty"), "  \0 \n").unwrap();
        assert_eq!(read_trimmed(&dir.join("model")).as_deref(), Some("Raspberry Pi 4 Model B Rev 1.4"));
        assert_eq!(read_trimmed(&dir.join("empty")), None);
        assert_eq!(read_trimmed(&dir.join("missing")), None);
    }

    #[test]
    fn this_system_gives_a_json_with_the_five_fields() {
        let value: Value = serde_json::from_str(json()).unwrap();
        let mut keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["brand", "manufacturer", "model", "sdk", "version"]);
        assert!(!value["version"].as_str().unwrap().is_empty());
        assert!(!value["sdk"].as_str().unwrap().is_empty(), "the kernel release");
        assert_eq!(soap_escape(json()), json_for_soap());
    }
}
