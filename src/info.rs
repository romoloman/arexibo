//! Player info served at `/info` (`xiboIC.info()`), with the same fields as
//! the official Electron player (`electron-player`, `src/main/express.ts`).

use once_cell::sync::Lazy;
use serde::Serialize;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerInfo {
    pub version: String,
    pub display_name: String,
    pub hardware_key: String,
    pub screen_width: i32,
    pub screen_height: i32,
    pub longitude: f64,
    pub latitude: f64,
    pub time_zone: String,
    pub current_layout_id: i64,
    /// CMS registration `status`: 0 ready, 1 added, 2 waiting for authorisation.
    pub display_status: i32,
}

impl Default for PlayerInfo {
    fn default() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            display_name: String::new(),
            hardware_key: String::new(),
            screen_width: 0,
            screen_height: 0,
            // No location source on Linux (the Electron player reports 0 too).
            longitude: 0.0,
            latitude: 0.0,
            time_zone: String::new(),
            current_layout_id: 0,
            // "Not registered yet", as in the Electron player.
            display_status: 2,
        }
    }
}

static INFO: Lazy<Mutex<PlayerInfo>> = Lazy::new(|| Mutex::new(PlayerInfo::default()));

fn update(f: impl FnOnce(&mut PlayerInfo)) {
    f(&mut INFO.lock().unwrap_or_else(|e| e.into_inner()));
}

pub fn set_identity(display_name: &str, hardware_key: &str, time_zone: &str) {
    update(|i| {
        i.display_name = display_name.to_string();
        i.hardware_key = hardware_key.to_string();
        i.time_zone = time_zone.to_string();
    });
}

pub fn set_screen_size(width: i32, height: i32) {
    update(|i| {
        i.screen_width = width;
        i.screen_height = height;
    });
}

/// The screen size, (0, 0) until the window has reported it.
pub fn screen_size() -> (i32, i32) {
    let info = INFO.lock().unwrap_or_else(|e| e.into_inner());
    (info.screen_width, info.screen_height)
}

pub fn set_current_layout(layout_id: i64) {
    update(|i| i.current_layout_id = layout_id);
}

pub fn set_display_status(status: i32) {
    update(|i| i.display_status = status);
}

pub fn render(info: &PlayerInfo) -> String {
    serde_json::to_string(info).expect("serializing player info")
}

pub fn json() -> String {
    render(&INFO.lock().unwrap_or_else(|e| e.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn json_has_exactly_the_electron_player_fields() {
        let v: Value = serde_json::from_str(&render(&PlayerInfo::default())).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["currentLayoutId", "displayName", "displayStatus", "hardwareKey",
                          "latitude", "longitude", "screenHeight", "screenWidth", "timeZone",
                          "version"]);
    }

    #[test]
    fn values_keep_the_types_widgets_expect() {
        let info = PlayerInfo {
            display_name: "totem-014".into(), hardware_key: "abc123".into(),
            screen_width: 1080, screen_height: 1920, time_zone: "Europe/Rome".into(),
            current_layout_id: 1737, display_status: 0, ..PlayerInfo::default()
        };
        let v: Value = serde_json::from_str(&render(&info)).unwrap();
        assert_eq!(v["displayName"], "totem-014");
        assert_eq!(v["hardwareKey"], "abc123");
        assert_eq!(v["screenWidth"], 1080);
        assert_eq!(v["screenHeight"], 1920);
        assert_eq!(v["timeZone"], "Europe/Rome");
        assert_eq!(v["currentLayoutId"], 1737);
        assert_eq!(v["displayStatus"], 0);
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert!(v["latitude"].is_number() && v["longitude"].is_number());
    }

    #[test]
    fn defaults_mean_not_registered_and_no_location() {
        let info = PlayerInfo::default();
        assert_eq!(info.display_status, 2);
        assert_eq!((info.latitude, info.longitude), (0.0, 0.0));
        assert_eq!(info.current_layout_id, 0);
    }
}
