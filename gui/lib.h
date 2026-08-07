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

void setup(const char *base_uri, const char *screen,
           int inspect, int debug, callback cb, void *cb_ptr);
void run();
void navigate(const char *file);
// max_width: downscale to this width (preserving aspect ratio) before
// submission, 0 = no resize -- see PlayerSettings::screenshot_size.
void screenshot(int max_width);
void set_title(const char *title);
void set_size(int pos_x, int pos_y, int size_x, int size_y);
void run_js(const char *js);
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
