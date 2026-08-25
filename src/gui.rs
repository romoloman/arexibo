// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Bindings to the C++/Qt GUI part of the application.

use std::ffi::{c_void, CStr, CString};
use std::sync::Arc;
use crossbeam_channel::{Sender, Receiver};
use parking_lot::Mutex;
use crate::config::PlayerSettings;
use crate::mainloop::{ToGui, FromGui, Kill};
use crate::resource::LayoutId;
use crate::server;

#[path = "qt_binding.rs"]
#[allow(non_camel_case_types)]
mod cpp;

struct CallbackData {
    sender: Sender<FromGui>,
    schedule: Arc<Mutex<Schedule<LayoutId>>>,
}

pub fn run(settings: PlayerSettings, screen: String, inspect: bool, debug: bool,
           togui: Receiver<ToGui>, fromgui: Sender<FromGui>) {
    let base_uri = format!("http://localhost:{}/", settings.embedded_server_port);
    let fromgui_2 = fromgui.clone();

    let schedule = Arc::new(Mutex::new(Schedule::<LayoutId>::default()));

    let cb_data = CallbackData { sender: fromgui_2, schedule: schedule.clone() };
    let cb_data = (Box::leak(Box::new(cb_data)) as *mut CallbackData).cast();

    let title = CString::new(settings.display_name).unwrap();
    let base_uri = CString::new(base_uri).unwrap();
    let screen = CString::new(screen).unwrap();
    unsafe {
        cpp::setup(base_uri.as_ptr(), screen.as_ptr(),
                   inspect.into(), debug.into(), Some(callback), cb_data);
        cpp::set_title(title.as_ptr());
        cpp::set_size(settings.pos_x as _, settings.pos_y as _,
                      settings.size_x as _, settings.size_y as _);
    }

    std::thread::spawn(move || {
        // Tracks the latest known PlayerSettings::screenshot_size (updated
        // whenever ToGui::Settings arrives) so ToGui::Screenshot can pass
        // the CMS-configured max width through to the actual capture --
        // previously this was completely ignored, always submitting the
        // screenshot at full captured resolution regardless of what the
        // Display Profile requested.
        let mut screenshot_size = settings.screenshot_size;
        for msg in togui {
            match msg {
                ToGui::Screenshot => {
                    // Diagnostic log (found genuinely useful
                    // investigating a real report: screenshots not
                    // reaching the CMS with --debug active, no error
                    // anywhere) -- confirms the cross-thread message
                    // from mainloop.rs actually arrives here, and that
                    // cpp::screenshot() is genuinely being called.
                    log::debug!("received screenshot request, capturing at max_width={screenshot_size}");
                    unsafe { cpp::screenshot(screenshot_size as i32); }
                }
                ToGui::Settings(s) => {
                    let title = CString::new(s.display_name).unwrap();
                    screenshot_size = s.screenshot_size;
                    unsafe {
                        cpp::set_title(title.as_ptr());
                        cpp::set_size(s.pos_x as _, s.pos_y as _, s.size_x as _, s.size_y as _);
                    }
                }
                ToGui::Layouts(new_layouts) => {
                    if let Some(id) = schedule.lock().update(new_layouts) {
                        log::info!("new schedule, showing layout: {id}");
                        let file = CString::new(format!("{id}.xlf.html")).unwrap();
                        unsafe {
                            cpp::navigate(file.as_ptr());
                        }
                    }
                }
                ToGui::WebHook(code) => {
                    let code = CString::new(format!(
                        "window.arexibo.trigger(\"{code}\");")).unwrap();
                    unsafe {
                        cpp::run_js(code.as_ptr());
                    }
                }
                ToGui::ReloadWidget(id) => {
                    // Only reload this one iframe in place -- not a full
                    // layout navigate(), which would restart every other
                    // widget's own playback/cycling state unnecessarily.
                    // Guards for the element missing (e.g. widget isn't
                    // on the currently displayed layout at all) and for
                    // it not being an iframe (server-rendered resource
                    // widgets always are -- see write_media's `Some("html")`
                    // branch in layout.rs -- so this is just defensive).
                    //
                    // Re-assigns src instead of calling
                    // el.contentWindow.location.reload(true) -- widgets
                    // are sharded across loopback origins (see
                    // server::HTML_SHARD_COUNT), so an iframe can be
                    // cross-origin, and contentWindow access throws a
                    // same-origin SecurityError. Re-assigning src needs
                    // no cross-origin access; cleared first so an
                    // unchanged value isn't treated as a no-op.
                    let code = CString::new(format!(
                        "(function() {{ \
                           var el = document.getElementById('m{id}'); \
                           if (el && el.tagName === 'IFRAME') {{ \
                             var src = el.src; \
                             el.src = ''; \
                             el.src = src; \
                           }} \
                         }})();")).unwrap();
                    unsafe {
                        cpp::run_js(code.as_ptr());
                    }
                }
                ToGui::ShowOverlay(id) => {
                    let file = CString::new(format!("{id}.xlf.html")).unwrap();
                    unsafe {
                        cpp::overlay_show(file.as_ptr());
                    }
                }
                ToGui::HideOverlay => {
                    unsafe {
                        cpp::overlay_hide();
                    }
                }
                ToGui::ControlDuration(req) => {
                    // Matches layout.rs's `controlDuration(widgetId, action,
                    // durationSecs)` -- action is passed as a plain string
                    // since there's no need for anything fancier here.
                    let action = match req.action {
                        server::DurationAction::Set => "set",
                        server::DurationAction::Extend => "extend",
                        server::DurationAction::Expire => "expire",
                    };
                    let code = CString::new(format!(
                        "window.arexibo.controlDuration({}, \"{action}\", {});",
                        req.widget_id, req.duration.unwrap_or(0))).unwrap();
                    unsafe {
                        cpp::run_js(code.as_ptr());
                    }
                }
                ToGui::Trigger(trigger_code) => {
                    // Matches layout.rs's own `write_action` -- a
                    // triggerType="webhook" action registers itself as
                    // `window.arexibo.triggers[code]`, only in the DOM
                    // of whichever page actually has that action (see
                    // TriggerRequest's own doc comment for why this
                    // means widget-scoping doesn't need a separate
                    // check here). A non-matching or not-currently-
                    // loaded code simply does nothing -- not an error,
                    // an external system firing a trigger nobody's
                    // listening for right now is a normal occurrence,
                    // not a bug.
                    let escaped = serde_json::to_string(&trigger_code).unwrap();
                    let code = CString::new(format!(
                        "if (window.arexibo.triggers[{escaped}]) window.arexibo.triggers[{escaped}]();"
                    )).unwrap();
                    unsafe {
                        cpp::run_js(code.as_ptr());
                    }
                }
            }
        }
    });


    unsafe {
        cpp::run();
    }
}

/// Signals Qt's own event loop (blocking inside run()'s own
/// `cpp::run()` call, on whichever thread called `run()`) to exit
/// cleanly -- thread-safe by Qt's own design (see cpp::quit's own doc
/// comment in gui/lib.h), meant to be called from a *different*
/// thread than the one running `run()` (typically the mainloop thread,
/// see main.rs's own shutdown sequencing) once that thread has decided
/// the whole process should exit. Calling std::process::exit()
/// directly instead, while Qt/Chromium are still fully active, was
/// causing a real, reproducible segfault on shutdown.
pub fn quit() {
    unsafe {
        cpp::quit();
    }
}

extern "C" fn callback(ptr: *mut c_void, typ: isize, arg1: isize, arg2: isize, _arg3: isize) {
    let cb_data = unsafe { &*(ptr as *const CallbackData) };

    match typ {
        cpp::CB_SCREENSHOT => {
            let data = unsafe { std::slice::from_raw_parts(arg1 as *const u8, arg2 as usize) };
            let _ = cb_data.sender.send(FromGui::Screenshot(data.to_vec()));
        }
        cpp::CB_LAYOUT_INIT => {
            if arg1 > 0 {  // don't announce the splash screen
                let _ = cb_data.sender.send(FromGui::Showing(arg1 as _));
            }
        }
        cpp::CB_LAYOUT_NEXT => {
            let mut schedule = cb_data.schedule.lock();
            if let Some(id) = schedule.next() {
                log::info!("showing next layout: {id}");
                let file = CString::new(format!("{id}.xlf.html")).expect("ok");
                unsafe {
                    cpp::navigate(file.as_ptr());
                }
            } else {
                schedule.mark_done();
            }
        }
        cpp::CB_LAYOUT_PREV => {
            if let Some(id) = cb_data.schedule.lock().prev() {
                log::info!("showing previous layout: {id}");
                let file = CString::new(format!("{id}.xlf.html")).expect("ok");
                unsafe {
                    cpp::navigate(file.as_ptr());
                }
            }
        }
        cpp::CB_LAYOUT_JUMP => {
            log::info!("jumping to layout: {arg2}");
            let file = CString::new(format!("{arg2}.xlf.html")).expect("ok");
            unsafe {
                cpp::navigate(file.as_ptr());
            }
        }
        cpp::CB_COMMAND | cpp::CB_SHELL => {
            let cmd = unsafe { CStr::from_ptr(arg1 as *const _) };
            let cmd = cmd.to_str().unwrap_or_default().to_owned();
            if typ == cpp::CB_SHELL {
                let use_shell = arg2 != 0;
                let _ = cb_data.sender.send(FromGui::Shell(cmd, use_shell));
            } else {
                let _ = cb_data.sender.send(FromGui::Command(cmd));
            }
        }
        cpp::CB_STOPSHELL => {
            let killmode = match arg1 & 0xff {
                0 => Kill::No,
                1 => Kill::Terminate,
                _ => Kill::Kill,
            };
            let _ = cb_data.sender.send(FromGui::StopShell(killmode));
        }
        cpp::CB_OVERLAY_LAYOUT_INIT => {
            // Deliberately not wired to anything -- unlike CB_LAYOUT_INIT,
            // this must NOT set current_layout (see FromGui::Showing) or
            // touch the main Schedule<T> cycling state, since the overlay
            // is not part of the normal schedule at all. Logged purely
            // for diagnostics.
            if arg1 > 0 {
                log::info!("overlay layout {arg1} initialized");
            }
        }
        _ => {
            log::warn!("got unknown callback from Qt: {typ}");
        }
    }
}

/// Keeps track of scheduled layouts and the currently shown one.
#[derive(Debug, Default)]
struct Schedule<T> {
    index: Option<usize>,
    layouts: Vec<T>,
    single_done: bool,
}

impl<T: Eq + Default + Clone> Schedule<T> {
    /// Update the scheduled layouts and return Some(id) if we need to change
    fn update(&mut self, new: Vec<T>) -> Option<T> {
        // determine the currently shown layout
        let cur_t = self.current();
        self.layouts = new;

        // if this layout is also in the new schedule, keep it
        if let Some(new_index) = self.layouts.iter().position(|t| t == &cur_t) {
            let next_index = if self.single_done {
                (new_index + 1) % self.layouts.len()
            } else {
                new_index
            };
            self.single_done = false;
            self.index = Some(next_index);
            // `single_done` can advance `next_index` to a layout other
            // than the one currently on screen (the single scheduled
            // layout just finished a full loop, and there's now more
            // than one candidate to cycle through) -- BUG FIXED: this
            // used to unconditionally return `None` here, meaning the
            // caller (gui.rs's ToGui::Layouts handler) never told the
            // browser to actually navigate whenever this happened, so
            // the internal index silently drifted away from what was
            // really being displayed, and the display could get stuck
            // showing a stale layout indefinitely even as the schedule
            // kept changing around it. Comparing against `cur_t` (the
            // layout actually shown before this call) is what a plain
            // index comparison can't do, since `self.index` itself was
            // just overwritten above.
            let next_t = self.layouts[next_index].clone();
            (next_t != cur_t).then_some(next_t)
        } else if !self.layouts.is_empty() {
            // otherwise, start showing the first of the new layouts if we have some
            self.index = Some(0);
            Some(self.layouts[0].clone())
        } else {
            // as last resort, show the splash screen
            self.index = None;
            Some(Default::default())
        }
    }

    /// Go to the next layout, if more than one is scheduled, and return Some(id)
    fn next(&mut self) -> Option<T> {
        let nlayouts = self.layouts.len();
        // if there is no layout or only one scheduled, no change
        if nlayouts < 2 {
            None
        } else {
            // otherwise just go further in the schedule
            let new_index = (self.index.expect("exists") + 1) % nlayouts;
            self.index = Some(new_index);
            Some(self.layouts[new_index].clone())
        }
    }

    /// Go to the previous layout, if more than one is scheduled, and return Some(id)
    fn prev(&mut self) -> Option<T> {
        let nlayouts = self.layouts.len();
        // if there is no layout or only one scheduled, no change
        if nlayouts < 2 {
            None
        } else {
            // otherwise just go further in the schedule
            let new_index = (self.index.expect("exists") + nlayouts - 1) % nlayouts;
            self.index = Some(new_index);
            Some(self.layouts[new_index].clone())
        }
    }

    /// Return current layout.
    fn current(&self) -> T {
        self.index.map(|i| self.layouts[i].clone()).unwrap_or_default()
    }

    /// Mark current layout as having run.
    fn mark_done(&mut self) {
        self.single_done = true;
    }
}

#[cfg(test)]
#[test]
fn test_schedule() {
    let mut schedule = Schedule { index: None, layouts: vec![], single_done: false };
    assert_eq!(schedule.next(), None);
    assert_eq!(schedule.update(vec![]), Some(0));
    assert_eq!(schedule.update(vec![1]), Some(1));
    assert_eq!(schedule.update(vec![1]), None);
    assert_eq!(schedule.update(vec![2, 1, 3]), None);
    assert_eq!(schedule.next(), Some(3));
    assert_eq!(schedule.next(), Some(2));
    assert_eq!(schedule.update(vec![1, 3]), Some(1));
}

/// Regression test for the bug reported in production: a single scheduled
/// layout (627) looping repeatedly sets `single_done` every time it
/// finishes a cycle (via `next()` returning `None` because there's only
/// one layout). When the schedule then gains a second layout (605) that
/// also contains 627, `update()` used to silently move `index` to the
/// other layout while still returning `None` -- so the browser was never
/// told to navigate, and the display got stuck on the old layout forever,
/// even across subsequent schedule changes (since `current()` from then
/// on reported the *new*, never-actually-shown layout as current).
#[cfg(test)]
#[test]
fn test_schedule_single_done_triggers_navigation() {
    let mut schedule = Schedule { index: Some(0), layouts: vec![627], single_done: false };
    // layout 627 finishes a full loop while it's the only one scheduled
    assert_eq!(schedule.next(), None);
    schedule.mark_done();
    // CMS now also schedules 605 alongside 627 -- must actually switch to
    // showing 605 (previously this returned None and never navigated)
    assert_eq!(schedule.update(vec![605, 627]), Some(605));
    // next collection: 627 drops out of the schedule entirely, only 605
    // remains -- since 605 was already the displayed layout, no further
    // navigation should be triggered
    assert_eq!(schedule.update(vec![605]), None);
}
