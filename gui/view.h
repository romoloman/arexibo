#ifndef AREXIBO_VIEW_H
#define AREXIBO_VIEW_H

#include <QMainWindow>
#include <QScreen>
#include <QMap>
#include <QtWebEngineWidgets/QWebEngineView>
#include <QtWebEngineCore/QWebEnginePage>
#include <QtWebEngineCore/QWebEngineScript>
#include <QtWebEngineCore/QWebEngineScriptCollection>
#include <QtWebChannel/QWebChannel>
#include <iostream>
#include <cstdlib>

#include "lib.h"

// Logs every JS `console.*` message from a page to arexibo's own stdout
// (same "INFO :"/"WARN :" convention as the rest of gui/*.cpp), including
// messages from *any iframe* within that page -- Chromium's own
// javaScriptConsoleMessage callback fires for the whole page's frame
// tree, not just the top-level document, so setting this on `view`
// alone already covers every widget iframe inside it too (no need to
// hook each iframe separately). Added as a lightweight, always-on
// diagnostic aid (found genuinely useful investigating a real Dataset
// View widget rendering problem: the *first* concrete evidence of
// what's actually failing inside a widget's own JS, without needing
// network-reachable remote debugging -- which, on a real totem where
// the debugging port only binds to 127.0.0.1 and SSH port forwarding
// wasn't reliable, wasn't a practical option at all).
// Controlled by --web-debug (see main.rs's Args -- "Enable debug
// logging of WebEngine messages", threaded through as gui::run's own
// `debug` parameter): whether LoggingPage actually prints anything, and
// whether `window.arexiboDebug` gets injected into every frame (see
// setup()) so JS-side diagnostics (layout.rs's own `arexibo-show:`
// console.log, and the shrink-to-fit script's `arexibo-shrink:` one) are
// gated the same way -- previously all unconditional, adding permanent
// noise to every single run regardless of whether anyone actually
// wanted this level of detail. A single flag, not a whole new one, on
// request: this one's own existing description already matched exactly
// what these do.
static bool g_web_debug_enabled = false;

class LoggingPage : public QWebEnginePage
{
    Q_OBJECT
public:
    LoggingPage(QObject *parent = nullptr) : QWebEnginePage(parent)
    {
        // BUG fix (found from a real report: a touch controller
        // intermittently reporting more simultaneous touch points than
        // Chromium's own hardcoded 16-point limit -- an out-of-bounds
        // std::array access deep inside Blink's own touch event
        // handling, std::array<blink::WebTouchPoint, 16>::operator[] --
        // crashed the *renderer* process specifically, on only one
        // totem, intermittently, consistent with a flaky touch
        // controller/cable/grounding issue rather than a genuine
        // 17-finger touch or a systemic software bug). Chromium's own
        // multi-process architecture means this doesn't necessarily
        // bring down arexibo's own top-level process at all -- the
        // GUI's own event loop keeps running, but the QWebEngineView
        // this page belongs to is left showing nothing further (a
        // black screen, reported directly), with no built-in recovery
        // of its own. Connecting to renderProcessTerminated here
        // (rather than at each individual call site) covers every view
        // that uses LoggingPage -- the main view, the overlay view, and
        // every render="native" widget view -- uniformly, in one place.
        //
        // Deliberately exits the *entire* arexibo process outright,
        // rather than trying to reload just the affected page/view in
        // place: a renderer crash this deep inside Chromium's own
        // internals isn't something we have any reliable way to
        // recover from at our level (e.g. Chromium's own GPU/compositor
        // state, shared across every view in the process, could easily
        // be left in a similarly bad state too) -- and arexibo.service
        // already has `Restart=always` (see arexibo.service itself),
        // so a clean, deliberate exit here is picked up automatically,
        // giving a genuinely fresh Xorg + arexibo + Chromium process
        // tree rather than a totem stuck on a black screen indefinitely
        // until someone manually intervenes on site.
        connect(this, &QWebEnginePage::renderProcessTerminated,
                [](QWebEnginePage::RenderProcessTerminationStatus status, int exitCode) {
            std::cout << "ERROR: [arexibo::qt] renderer process terminated "
                       << "(status=" << static_cast<int>(status) << ", exitCode=" << exitCode
                       << ") -- exiting so systemd (Restart=always) can start a fresh instance"
                       << std::endl;
            std::exit(1);
        });
    }

protected:
    void javaScriptConsoleMessage(JavaScriptConsoleMessageLevel level, const QString &message,
                                   int lineNumber, const QString &sourceID) override
    {
        if (!g_web_debug_enabled) return;
        const char *tag = level == ErrorMessageLevel ? "WARN " : "INFO ";
        std::cout << tag << ": [arexibo::qt] JS console [" << sourceID.toStdString()
                   << ":" << lineNumber << "] " << message.toStdString() << std::endl;
    }
};

class Window : public QMainWindow
{
    Q_OBJECT
    friend class JSInterface;

public:
    Window(QString, QScreen *, int, callback, void *);

private:
    QWebEngineView *view;
    QWebChannel *channel;
    QString base_uri;
    QScreen *selected_screen;

    callback cb;
    void *cb_ptr;

    int layout_width;
    int layout_height;

    // One additional, non-iframed QWebEngineView per `render="native"`
    // webpage widget currently on screen in the MAIN layout -- keyed by
    // the widget's XLF media id. See jsNativeWebShowImpl/jsNativeWebHideImpl
    // in view.cpp for why this exists instead of just using an iframe
    // (X-Frame-Options and similar frame-busting headers only block
    // *embedding*, not a real top-level browser view).
    QMap<int, QWebEngineView*> native_views;

    // --- Overlay layout support (XMR `overlayLayout` action) ---
    // A second, independent QWebEngineView/QWebChannel/JSInterface stack,
    // shown on top of the main view without interrupting it (unlike
    // `changeLayout`, which replaces what's on screen -- see
    // xmr::Message::OverlayLayout/ChangeLayout in mainloop.rs). Lazily
    // created on first use via ensureOverlayView(); torn down (not just
    // hidden) in overlayHideImpl() so an idle overlay doesn't keep a live
    // QWebEngineView/renderer process around indefinitely between uses.
    QWebEngineView *overlay_view = nullptr;
    QWebChannel *overlay_channel = nullptr;
    int overlay_layout_width = 1920;
    int overlay_layout_height = 1080;
    // Own set of native_views for `webpage render="native"` widgets that
    // happen to be inside the overlay layout itself -- kept separate
    // from the main view's `native_views` so overlayHideImpl() tears down
    // exactly its own without touching the main layout's.
    QMap<int, QWebEngineView*> overlay_native_views;

    void ensureOverlayView();

    void adjustScale(int, int);
    void adjustOverlayScale(int, int);

public:
    // `overlay` selects which view/geometry/native-view-map a call
    // applies to -- see jsNativeWebShow/jsNativeWebHide in JSInterface,
    // which is itself bound to a specific view via its own `is_overlay`
    // flag and passes it straight through.
    void jsNativeWebShowImpl(bool overlay, int, QString, int, int, int, int);
    void jsNativeWebHideImpl(bool overlay, int);
    // Destroys any native_views left over from the *previous* layout --
    // called before navigating to a new one, since those widgets don't
    // exist in the new page at all and would otherwise leak/linger on
    // screen indefinitely.
    void clearNativeViews(bool overlay);

signals:
    void navigateTo(QString);
    void screenShot(int max_width);
    void setTitle(QString);
    void setSize(int, int, int, int);
    void runJavascript(QString);
    void overlayShow(QString);
    void overlayHide();

public slots:
    void navigateToImpl(QString);
    void screenShotImpl(int max_width);
    void setSizeImpl(int, int, int, int);
    void runJavascriptImpl(QString);
    void overlayShowImpl(QString);
    void overlayHideImpl();
};

class JSInterface : public QObject
{
    Q_OBJECT

public:
    // `is_overlay` distinguishes which view/QWebChannel this instance is
    // bound to -- see the members it guards in view.cpp (e.g. jsLayoutInit
    // must NOT report the overlay's own layout id as "the current layout"
    // to the CMS; jsLayoutDone has no cross-layout cycling to do for a
    // standalone overlay).
    JSInterface(Window *wnd, bool is_overlay = false) :
        QObject(wnd), wnd(wnd), is_overlay(is_overlay) {}

private:
    Window *wnd;
    bool is_overlay;

public slots:
    void jsLayoutInit(int, int, int);
    void jsLayoutDone(int);
    void jsLayoutPrev(int);
    void jsLayoutJump(int, int);
    void jsCommand(QString);
    void jsShell(QString, int);
    void jsStopShell(int);
    void jsNativeWebShow(int, QString, int, int, int, int);
    void jsNativeWebHide(int);
};

#endif
