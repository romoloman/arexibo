#ifndef AREXIBO_LIB_H
#define AREXIBO_LIB_H

#include <stdint.h>

extern "C" {

typedef void (* callback)(void *cb_ptr, intptr_t cb_type,
                          intptr_t arg1, intptr_t arg2, intptr_t arg3);

const intptr_t CB_LAYOUT_INIT = 1;
const intptr_t CB_LAYOUT_NEXT = 2;
const intptr_t CB_LAYOUT_PREV = 3;
const intptr_t CB_LAYOUT_JUMP = 4;
const intptr_t CB_SCREENSHOT  = 5;
const intptr_t CB_COMMAND     = 6;
const intptr_t CB_SHELL       = 7;
const intptr_t CB_STOPSHELL   = 8;
// Distinct from CB_LAYOUT_INIT: fired when the *overlay* view's own
// translated layout finishes loading, not the main one -- must NOT be
// treated as "the current layout" for CMS status reporting purposes
// (see FromGui::Showing in mainloop.rs, only wired to CB_LAYOUT_INIT).
const intptr_t CB_OVERLAY_LAYOUT_INIT = 9;
// The main view's page failed to load several times in a row (arg1: the
// layout id from its /<id>.xlf.html URL) -- e.g. a layout that was purged
// while a stale page still linked to it. The view keeps retrying on its
// own; this lets the Rust side move it elsewhere.
const intptr_t CB_LAYOUT_FAILED = 10;
// The window's size in real pixels (arg1: width, arg2: height).
const intptr_t CB_SCREEN_SIZE = 11;

void setup(const char *base_uri, const char *screen,
           int inspect, int debug, callback cb, void *cb_ptr);
void run();
// Thread-safe by Qt's own design (QCoreApplication::quit() posts a
// quit event internally, safe to call from any thread) -- lets
// run()'s own blocking the_app->exec() return cleanly instead of the
// process terminating abruptly (std::process::exit()) while Qt/
// Chromium are still fully active, which was causing a real,
// reproducible segfault on shutdown (found from a real report).
void quit();
void navigate(const char *file);
// max_width: downscale to this width (preserving aspect ratio) before
// submission, 0 = no resize -- see PlayerSettings::screenshot_size.
void screenshot(int max_width);
void set_title(const char *title);
void set_size(int pos_x, int pos_y, int size_x, int size_y);
void run_js(const char *js);
// Data Connectors: a hidden page of its own per connector (see src/dcruntime.rs).
void data_connector_start(int data_set_id, const char *url);
void data_connector_stop(int data_set_id);
// Show a layout as an overlay on top of whatever the main view is
// currently displaying, without interrupting it -- XMR `overlayLayout`
// action (see xmr::Message::OverlayLayout in mainloop.rs). `file` is the
// translated layout HTML filename, same convention as navigate().
void overlay_show(const char *file);
// Tear down the overlay (if any) and go back to showing only the main
// view -- XMR `revertToSchedule`, or automatically once the overlay's
// requested duration elapses (see mainloop.rs).
void overlay_hide();

}

#endif
