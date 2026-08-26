# Arexibo

<p align="center">
  <img src="https://github.com/birkenfeld/arexibo/blob/master/assets/logo.png?raw=true" alt="Logo"/>
</p>

Arexibo is an unofficial alternate Digital Signage Player for [Xibo](https://xibo.org.uk),
implemented mostly in Rust but making use of Qt GUI components, for Linux platforms.

It is currently almost complete but there are still some features that may present errors or that maybe are not perfectly working, in particular:
VideoWall/SyncGroup and Cycle Playback (also not working properly in the windows client)

All the other settings from the cms should work including allow_wan_access and embedded_server_port.


## Installation

A nightly build is provided but it's better to compile from source.

To build from source, you need:

* The [Rust toolchain](https://www.rust-lang.org/), version >= 1.75.  Refer to
  https://rustup.rs/ for the easiest way to install, if the Linux distribution
  provided package is too old.

* CMake and a C++ compiler.

* Qt 6 with the QtWebEngine component and its development headers.

* Development headers for `dbus` (>= 1.6), `zeromq` (>= 4.1)
  as well as `pkg-config`.

To build, run:

```
$ cargo build --release
```

The binary is placed in `target/release/arexibo` and can be run from there.

To install, run:

```
$ cargo install --path . --root /usr
```

The will install the binary to `/usr/bin/arexibo`.  It requires no other files
at runtime, except for the system libraries it is linked against.

Builds have been tested with the available dependency library versions on Fedora
41, RHEL 9 with EPEL and Ubuntu 24.04.  Note that in order to play some media
like mp4 videos, you will require a `ffmpeg` package that includes some codecs
that RHEL/Fedora don't include in their packages, e.g. from rpmfusion.org.

For RHEL derived distributions, install `cmake gcc-c++ cargo dbus-devel
zeromq-devel qt6-qtwebengine-devel`.  For Debian derived, install `cmake g++
cargo libdbus-1-dev libzmq3-dev qt6-webengine-dev`.


## Usage

Create a new directory where Arexibo can store configuration and media files.
Then, at first start, use the following command line to configure the player:

```
arexibo --host <https://my.cms/> --key <key> <dir>
```

Further configuration options are `--display-id` (which is normally
auto-generated from machine characteristics) and `--proxy` (if needed).

Arexibo will cache the configuration in the directory, so that in the future you
only need to start with

```
arexibo <dir>
```

Log messages are printed to stdout.  The GUI window will only show up once the
display is authorized.


## Standalone setup with X server

The following example systemd service file shows how to to start an X server
with Arexibo and no DPMS/screensaver:

```
[Unit]
Description=Start X with Arexibo player
After=network-online.target
Requires=network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/xinit /usr/bin/arexibo /home/xibo/env -- :0 vt2 -s 0 -v -dpms
User=xibo
Restart=always
RestartSec=60
Environment=NO_AT_BRIDGE=1

[Install]
WantedBy=multi-user.target
```
## Useful flags for development

    --debug: verbose logging (SOAP calls, internal state transitions).
    --web-debug: logs every JS console message and page error from the rendered content – useful when troubleshooting a specific widget that isn’t displaying correctly.
    --allow-offline: tolerates the CMS being unreachable at startup, falling back to cached settings/content if available, instead of failing outright.
    --clear: wipes the local file cache (downloaded media/widget pages), forcing a full re-download on next start. Does not affect cached CMS connection settings, it's always recommended to run --clear once after a new git pull or git clone 
    --no-verify: skips TLS certificate verification – only for testing against a CMS with a self-signed certificate, never for production.

## Some useful environment variables

Normal use:

    AREXIBO_FONT_SCALE: a numeric multiplier (e.g. 0.91) applied globally to font sizes, to correct for rendering differences compared to a reference client on a different platform. Leave unset for the default (no correction).
    QTWEBENGINE_CHROMIUM_FLAGS: standard Qt/Chromium mechanism for passing extra Chromium command-line flags (e.g. GPU-related tuning for a specific graphics driver). arexibo appends its own required flag (--disable-pinch, needed to disable pinch-to-zoom on multitouch panels) to whatever you set here, rather than overwriting it – both apply together.
    QTWEBENGINE_REMOTE_DEBUGGING: QTWEBENGINE_REMOTE_DEBUGGING=9222 as an environment variable lets you inspect the rendered page from another machine’s Chrome/Edge at http://<host>:9222 (via an SSH tunnel if not on the same network) – useful for confirming content renders correctly without needing eyes on the actual totem screen.

Diagnostic only – not for production use:

    AREXIBO_FAKE_USERAGENT: overrides the user agent string reported to the CMS/widgets (e.g. windows to masquerade as a Windows client). Added specifically to investigate a rendering bug that turned out to be unrelated to the user agent at all – misrepresents the player to anything checking it, don’t leave this set normally.
    AREXIBO_FAKE_CLIENTTYPE: overrides the clientType reported to the CMS during registration (currently only windows is implemented as an override value; anything else falls back to the real linux type with a warning). Added to test a hypothesis about CMS-side behavior differing by client type – same caveat as above, this misrepresents the player to the CMS’s own registration logic, which can affect other clientType-conditional CMS behavior beyond whatever you’re specifically testing.
