//! A supervisor for the player on systems without systemd: `arexibo --watchdog
//! <options>` starts the player (itself, without `--watchdog`) and starts it
//! again whenever it exits, as `Restart=always` with `RestartSec=3` does for
//! the systemd service. After a registration with a code, a change of CMS or
//! of the way it is reached, and the exit of the startup check, the player
//! exits and expects that.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// How long to wait before the player is started again (`RestartSec=3`).
const RESTART_DELAY: Duration = Duration::from_secs(3);

/// The options of the player that take a value. A test in main.rs keeps this
/// list in step with `Args`.
pub const VALUE_OPTIONS: [&str; 6] =
    ["--host", "--key", "--display-id", "--display-name", "--proxy", "--screen"];

/// The options that write the player's own settings: given until those exist
/// (`cms.json`), not after, because given again they would undo a change of
/// CMS. All the others are given at every start.
pub const FIRST_START_OPTIONS: [&str; 5] =
    ["--host", "--key", "--display-id", "--display-name", "--clear"];

/// `--name` of `--name` and of `--name=value`.
fn option_name(arg: &OsStr) -> Option<String> {
    let text = arg.to_str()?.strip_prefix("--")?;
    if text.is_empty() {
        return None;
    }
    Some(format!("--{}", text.split('=').next().unwrap_or(text)))
}

fn has_inline_value(arg: &OsStr) -> bool {
    arg.to_str().map_or(false, |text| text.starts_with("--") && text.contains('='))
}

fn takes_next_value(arg: &OsStr, name: &str) -> bool {
    VALUE_OPTIONS.contains(&name) && !has_inline_value(arg)
}

pub fn without_watchdog(args: &[OsString]) -> Vec<OsString> {
    args.iter().filter(|arg| arg.as_os_str() != OsStr::new("--watchdog")).cloned().collect()
}

pub fn without_first_start_options(args: &[OsString]) -> Vec<OsString> {
    let mut kept = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match option_name(arg) {
            Some(name) if FIRST_START_OPTIONS.contains(&name.as_str()) => {
                if takes_next_value(arg, &name) {
                    iter.next();
                }
            }
            Some(name) if takes_next_value(arg, &name) => {
                kept.push(arg.clone());
                kept.extend(iter.next().cloned());
            }
            _ => kept.push(arg.clone()),
        }
    }
    kept
}

/// The directory of the player's files, if the arguments give it: the first
/// argument that is not an option or the value of one.
pub fn envdir_of(args: &[OsString]) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(name) = option_name(arg) {
            if takes_next_value(arg, &name) {
                iter.next();
            }
        } else if !arg.to_str().map_or(false, |text| text.starts_with('-')) {
            return Some(PathBuf::from(arg));
        }
    }
    None
}

/// The arguments as text for the log, without the CMS key.
pub fn describe(args: &[OsString]) -> String {
    let mut parts = Vec::new();
    let mut hide_next = false;
    for arg in args {
        let text = arg.to_string_lossy();
        if hide_next {
            parts.push("***".to_string());
            hide_next = false;
        } else if text == "--key" {
            hide_next = true;
            parts.push(text.into_owned());
        } else if text.starts_with("--key=") {
            parts.push("--key=***".to_string());
        } else {
            parts.push(text.into_owned());
        }
    }
    parts.join(" ")
}

pub fn exit_message(code: Option<i32>) -> String {
    match code {
        Some(0) => "exited".to_string(),
        Some(1) => "exited on an error".to_string(),
        Some(2) => "exited: the display is not authorized".to_string(),
        Some(3) => "exited asking to be started again".to_string(),
        Some(70) => "exited because its main loop did not start a collection".to_string(),
        Some(code) => format!("exited with code {code}"),
        None => "was ended without an exit code".to_string(),
    }
}

/// The loop, with the starting of the player, the pause and the decision to
/// go on as parameters, to be tried.
pub fn supervise(args: &[OsString], default_envdir: Option<PathBuf>,
                 mut run_player: impl FnMut(&[OsString]) -> io::Result<Option<i32>>,
                 mut pause: impl FnMut(Duration), mut again: impl FnMut() -> bool) {
    let full = without_watchdog(args);
    let reduced = without_first_start_options(&full);
    let envdir = envdir_of(&full).or(default_envdir);
    let mut first = true;
    loop {
        // Without a known directory, only the first start gets everything.
        let configured = envdir.as_ref().map_or(!first, |dir| dir.join("cms.json").exists());
        let player_args = if configured { &reduced } else { &full };
        log::info!("starting the player: arexibo {}", describe(player_args));
        match run_player(player_args) {
            Ok(code) => log::warn!("the player {}", exit_message(code)),
            Err(e) => log::error!("cannot start the player: {e}"),
        }
        first = false;
        if !again() {
            return;
        }
        log::info!("starting it again in {} seconds", RESTART_DELAY.as_secs());
        pause(RESTART_DELAY);
    }
}

pub fn run(args: Vec<OsString>, default_envdir: Option<PathBuf>) -> ! {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            log::error!("cannot find my own executable: {e}");
            std::process::exit(1);
        }
    };
    supervise(&args, default_envdir,
              |player_args| std::process::Command::new(&exe).args(player_args).status().map(|s| s.code()),
              std::thread::sleep, || true);
    unreachable!("the supervisor runs for ever")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("arexibo_watchdog_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_watchdog_option_is_not_passed_on() {
        assert_eq!(strings(&without_watchdog(&os(&["--watchdog", "--screen", "2", "dir"]))), ["--screen", "2", "dir"]);
        assert_eq!(strings(&without_watchdog(&os(&["dir", "--watchdog"]))), ["dir"]);
    }

    #[test]
    fn the_options_that_write_the_settings_are_dropped_with_their_values() {
        let args = os(&["--host", "https://cms", "--key", "k", "--display-id", "7", "--display-name", "My Totem",
                        "--clear", "--screen", "2", "--proxy", "http://p", "--web-debug", "dir"]);
        assert_eq!(strings(&without_first_start_options(&args)),
                   ["--screen", "2", "--proxy", "http://p", "--web-debug", "dir"]);
    }

    #[test]
    fn the_same_options_with_an_equals_sign_are_dropped_too() {
        let args = os(&["--host=https://cms", "--key=k", "--screen=2", "dir", "--display-name=My Totem"]);
        assert_eq!(strings(&without_first_start_options(&args)), ["--screen=2", "dir"]);
    }

    #[test]
    fn a_value_that_looks_like_an_option_is_still_a_value() {
        // the name of a display that is dropped must not leave its value behind, nor eat the directory
        let args = os(&["--display-name", "dir", "--screen", "1"]);
        assert_eq!(strings(&without_first_start_options(&args)), ["--screen", "1"]);
    }

    #[test]
    fn nothing_to_drop_changes_nothing() {
        let args = os(&["C:\\Users\\me\\Documents\\arexibo", "--debug"]);
        assert_eq!(without_first_start_options(&args), args);
        assert!(without_first_start_options(&[]).is_empty());
    }

    #[test]
    fn the_directory_is_the_argument_that_is_not_an_option_nor_its_value() {
        assert_eq!(envdir_of(&os(&["--screen", "2", "C:\\arexibo", "--debug"])), Some(PathBuf::from("C:\\arexibo")));
        assert_eq!(envdir_of(&os(&["--host=https://cms", "dir"])), Some(PathBuf::from("dir")));
        assert_eq!(envdir_of(&os(&["--host", "https://cms", "--key", "k"])), None);
        assert_eq!(envdir_of(&os(&["-h", "--debug"])), None, "a short option is not a directory");
        assert_eq!(envdir_of(&[]), None);
    }

    #[test]
    fn the_key_is_not_written_to_the_log() {
        assert_eq!(describe(&os(&["--host", "https://cms", "--key", "secret", "dir"])), "--host https://cms --key *** dir");
        assert_eq!(describe(&os(&["--key=secret", "dir"])), "--key=*** dir");
        assert_eq!(describe(&os(&["dir"])), "dir");
    }

    #[test]
    fn each_exit_is_told_in_words() {
        assert_eq!(exit_message(Some(3)), "exited asking to be started again");
        assert!(exit_message(Some(70)).contains("main loop"));
        assert!(exit_message(Some(2)).contains("not authorized"));
        assert_eq!(exit_message(Some(0)), "exited");
        assert_eq!(exit_message(Some(42)), "exited with code 42");
        assert!(exit_message(None).contains("without an exit code"));
    }

    #[test]
    fn everything_is_given_until_the_settings_exist_and_the_rest_after() {
        let dir = temp("loop");
        let dir_text = dir.to_str().unwrap().to_string();
        let args = os(&["--watchdog", "--host", "https://cms", "--key", "secret", "--screen", "2", &dir_text]);
        let (mut seen, mut pauses, mut rounds) = (Vec::new(), Vec::new(), 0);
        supervise(&args, None,
                  |a| {
                      seen.push(strings(a));
                      if seen.len() == 1 {
                          std::fs::write(dir.join("cms.json"), "{}").unwrap(); // the player saved its settings
                      }
                      Ok(Some(3))
                  },
                  |d| pauses.push(d),
                  || { rounds += 1; rounds < 3 });
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0], ["--host", "https://cms", "--key", "secret", "--screen", "2", &dir_text]);
        assert_eq!(seen[1], ["--screen", "2", &dir_text], "the CMS is no longer given");
        assert_eq!(seen[2], seen[1]);
        assert_eq!(pauses, vec![Duration::from_secs(3); 2], "3 seconds between the starts, none after the last");
    }

    #[test]
    fn a_registration_that_did_not_finish_keeps_its_address_and_key() {
        let dir = temp("unfinished");
        let dir_text = dir.to_str().unwrap().to_string();
        let args = os(&["--host", "https://cms", "--key", "secret", &dir_text]);
        let (mut seen, mut rounds) = (Vec::new(), 0);
        supervise(&args, None, |a| { seen.push(strings(a)); Ok(Some(1)) }, |_| {}, || { rounds += 1; rounds < 3 });
        assert_eq!(seen.len(), 3);
        assert!(seen.iter().all(|a| a.contains(&"secret".to_string())), "no cms.json yet: still given: {seen:?}");
    }

    #[test]
    fn the_default_directory_counts_when_none_is_given() {
        let dir = temp("default");
        std::fs::write(dir.join("cms.json"), "{}").unwrap();
        let args = os(&["--host", "https://cms", "--key", "secret", "--screen", "2"]);
        let (mut seen, mut rounds) = (Vec::new(), 0);
        supervise(&args, Some(dir), |a| { seen.push(strings(a)); Ok(Some(0)) }, |_| {}, || { rounds += 1; rounds < 2 });
        assert_eq!(seen[0], ["--screen", "2"], "the settings exist already: the CMS is not given again");
    }

    #[test]
    fn without_any_directory_only_the_first_start_gets_everything() {
        let args = os(&["--host", "https://cms", "--key", "secret"]);
        let (mut seen, mut rounds) = (Vec::new(), 0);
        supervise(&args, None, |a| { seen.push(strings(a)); Ok(Some(0)) }, |_| {}, || { rounds += 1; rounds < 3 });
        assert_eq!(seen[0], ["--host", "https://cms", "--key", "secret"]);
        assert!(seen[1].is_empty() && seen[2].is_empty(), "{seen:?}");
    }

    #[test]
    fn a_player_that_cannot_be_started_is_tried_again() {
        let (mut tries, mut pauses, mut rounds) = (0, 0, 0);
        supervise(&os(&["dir"]), None,
                  |_| { tries += 1; Err(io::Error::new(io::ErrorKind::NotFound, "no such file")) },
                  |_| pauses += 1, || { rounds += 1; rounds < 3 });
        assert_eq!((tries, pauses), (3, 2));
    }
}
