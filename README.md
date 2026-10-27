# Arexibo

<p align="center">
  <img src="https://github.com/birkenfeld/arexibo/blob/master/assets/logo.png?raw=true" alt="Logo"/>
</p>

Arexibo is an unofficial alternate Digital Signage Player for [Xibo](https://xibo.org.uk),
implemented mostly in Rust but making use of Qt GUI components, for Linux platforms and,
as a more recent and less tested port, for Windows (see [Windows](#windows-experimental)).

It is currently almost complete but there are still some features that may present errors or that maybe are not perfectly working, in particular:
VideoWall/SyncGroup and Cycle Playback (also not working properly in the windows client)

SyncGroup should work properly on normal layout but still require a better synchronisation for video.

All the other settings from the cms should work including allow_wan_access and embedded_server_port.


## Installation

Tagged releases and a nightly build provide `.deb` packages for amd64 and arm64, built on
Ubuntu 26.04, but it's better to compile from source.
For Windows they also provide an installer (see [Windows](#windows-experimental)).

To build from source, you need:

* The [Rust toolchain](https://www.rust-lang.org/), version >= 1.75.  Refer to
  https://rustup.rs/ for the easiest way to install, if the Linux distribution
  provided package is too old.

* CMake and a C++ compiler.

* Qt 6 with the QtWebEngine component and its development headers.

* On Linux, development headers for `dbus` (>= 1.6), `zeromq` (>= 4.1)
  as well as `pkg-config`.

For Windows, see [Windows](#windows-experimental) below.

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
41, RHEL 9 with EPEL and Ubuntu 26.04.  Note that in order to play some media
like mp4 videos, you will require a `ffmpeg` package that includes some codecs
that RHEL/Fedora don't include in their packages, e.g. from rpmfusion.org.

For RHEL derived distributions, install `cmake gcc-c++ cargo dbus-devel
zeromq-devel qt6-qtwebengine-devel`.  For Debian derived, install `cmake g++
cargo libdbus-1-dev libzmq3-dev qt6-webengine-dev`.


## Windows (experimental)

Arexibo also builds and runs on Windows (64 bit, MSVC toolchain).  This is a
recent port and less tested than the Linux one: see "Differences on Windows"
below.  The steps below are those of the workflow
`.github/workflows/windows.yml`, which builds it in the CI.

### Installing the tools

None of this is needed just to *get* a Windows build: the CI makes one (see
"Installer" below).  To build it yourself you need the following.  The commands
are for a command prompt or PowerShell; `winget` comes with current versions of
Windows 10 and 11.

1. **Rust**, with the default `x86_64-pc-windows-msvc` target.  Download and run
   `rustup-init.exe` from https://rustup.rs and accept the defaults.  The version
   is the one pinned in `rust-toolchain.toml`: in the directory of the sources,
   run `rustup toolchain install` (without arguments) to get it.

2. **The Visual Studio 2022 Build Tools**, with the C++ workload: the MSVC
   compiler, the Windows SDK and CMake.  You do not need the full Visual
   Studio.  This asks for administrator rights and downloads several gigabytes:

   ```
   > winget install --id Microsoft.VisualStudio.2022.BuildTools --override "--passive --wait --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.Windows11SDK.22621 --includeRecommended"
   ```

   `--includeRecommended` is what brings "C++ CMake tools for Windows".  If you
   already have Visual Studio 2022, add the workload "Desktop development with
   C++" with the Visual Studio Installer instead.  The commands below are meant
   for the "Developer Command Prompt for VS 2022" in the Start menu.

3. **Qt 6** built for MSVC 2022, 64 bit, with the QtWebEngine, QtWebChannel and
   QtPositioning modules (the CI uses Qt 6.8).  With
   [aqtinstall](https://github.com/miurahr/aqtinstall), which needs Python 3 and
   is what the CI uses:

   ```
   > py -m pip install aqtinstall
   > py -m aqt install-qt --outputdir C:\Qt windows desktop 6.8.3 win64_msvc2022_64 -m qtwebengine qtwebchannel qtpositioning
   ```

   The kit is then in `C:\Qt\6.8.3\msvc2022_64`.  `py -m aqt list-qt windows
   desktop --modules 6.8.3 win64_msvc2022_64` lists the modules, in case a
   version names them differently.  Or use the Qt Online Installer from
   https://www.qt.io/download-open-source (it needs a free Qt account): choose
   Qt 6.8, "MSVC 2022 64-bit", and under "Additional Libraries" Qt WebEngine,
   Qt WebChannel and Qt Positioning.

   Set `QT_ROOT_DIR` to the directory of the kit; the build script finds Qt's
   CMake files and libraries from it.  `setx QT_ROOT_DIR C:\Qt\6.8.3\msvc2022_64`
   keeps it for the prompts you open afterwards.

4. **Inno Setup 6**, only to make the installer: `choco install innosetup` if you
   use Chocolatey, or the installer from https://jrsoftware.org/isdl.php.

To check what is installed, in a new Developer Command Prompt:

```
> rustc --version
> cl
> cmake --version
> %QT_ROOT_DIR%\bin\windeployqt.exe --version
```

### Building

From a "Developer Command Prompt for VS 2022", run:

```
> set QT_ROOT_DIR=C:\Qt\6.8.3\msvc2022_64
> cargo build --release --locked
```

(If building the `aws-lc-sys` crate complains about NASM, install NASM.)

The binary `target\release\arexibo.exe` needs Qt's DLLs and plugins next to it.
`windeployqt` collects them; run it from the same Developer Command Prompt,
because `--compiler-runtime` needs the Visual Studio environment (it puts the
installer of the Visual C++ runtime in the folder too):

```
> mkdir dist-windows
> copy target\release\arexibo.exe dist-windows\
> %QT_ROOT_DIR%\bin\windeployqt --release --no-translations --compiler-runtime dist-windows\arexibo.exe
> dist-windows\arexibo.exe --help
```

### Running

Usage is the same as on Linux, with one difference: the directory can be left
out.  It is then `Documents\arexibo` (the Documents folder of the user, wherever
Windows has it), and it is created if it is missing.

```
> dist-windows\arexibo.exe --host https://my.cms/ --key <key>
```

The player exits when it needs to be started again: after the registration with
a code, when the CMS changes or the connection to it is set up differently.  On
Linux systemd starts it again (`Restart=always`).  On Windows use
`installer\arexibo-run.vbs`, which starts the player, waits for it to exit and
starts it again after 3 seconds:

```
> wscript installer\arexibo-run.vbs --host https://my.cms/ --key <key>
```

The arguments are for the first start only: after it the player has its own
settings, and giving them again would undo a change of CMS.  To stop the player
started like this, end `wscript.exe` in the Task Manager.

### Installer

The installer is made with [Inno Setup](https://jrsoftware.org/isinfo.php) 6
(`iscc` is `ISCC.exe` in its folder, which the installation of Inno Setup does not add to
the `PATH`):

```
> iscc /DAppVersion=0.6.9 /DSourceDir=%CD%\dist-windows installer\arexibo.iss
```

It writes `installer\output\arexibo-setup-0.6.9.exe` (`/DOutputName=<name>`
chooses another file name, without `.exe`).  The installer puts the program in
`Program Files\Arexibo`, makes shortcuts that go through `arexibo-run.vbs` (in
the Start menu, and on request on the desktop and at the sign-in of every
user), asks for the address of the CMS and its key, which are given to the
first start, and installs the Visual C++ runtime if the machine does not have it.
The files of the player go to `Documents\arexibo` of the user.

The workflow builds the installer; run it by hand from the Actions tab.  The
tagged releases and the nightly build attach it as
`arexibo-setup-<version>.exe` and `arexibo-setup-nightly.exe`.  It is not
signed, so Windows SmartScreen warns when it is run for the first time.

### Differences on Windows

* The timezone of the CMS is not applied: the player uses the one of the machine.
* A console window with the log messages stays open besides the player's window.
* The screensaver and the sleep of the machine are inhibited while the player
  runs.
* The display id (normally generated from characteristics of the machine), the
  commands that the CMS can run and the serial (RS232) ports follow Unix
  conventions and have not been adapted or tested on Windows.  Give
  `--display-id` explicitly if several machines could otherwise get the same one.

## Usage

Create a new directory where Arexibo can store configuration and media files
(on Windows the directory can be left out: see [Windows](#windows-experimental)).
Then, you can use several ways to configure the player:

* at first start, use the following command line 

```
arexibo --host <https://my.cms/> --key <key> <dir>
```

Further configuration options are `--display-id` (which is normally
auto-generated from machine characteristics), `--display-name` (the initial
name of the display), `--screen` (the screen to show Arexibo on: a number
starting from 1 or a name; `--screen list` lists the connected screens) and
`--proxy` (if needed).

* or launch

```
arexibo <dir>
```

then you are given the opportunity to enter cms address and key directly on the screen or user the code to register arexibo on the cms.

Arexibo will cache the configuration in the directory, so that in the future you
only need to start with

```
arexibo <dir>
```

Log messages are printed to stdout.  The GUI window will only show up once the
display is authorized.

## Other configuration parameters

* `--disallow-offline`: requires a live CMS connection, instead of the default of falling back to the cached settings/content (if available) when the CMS is unreachable. With it, a display that loses the connection later also falls back to the splash screen after 3 consecutive failed collection cycles; by default it keeps playing the cached content and keeps retrying.
* `--allow-offline`: deprecated and ignored (falling back to the cache is now the default); kept so existing scripts don't break.
* `--no-verify`: skips TLS certificate verification – only for testing against a CMS with a self-signed certificate, never for production.

## TLS certificates

Rust HTTPS requests (including CMS communication and media downloads) and XMR
WebSocket connections trust both the bundled Mozilla roots and the Linux system
CA store, so a CMS behind an internal CA works without `--no-verify`. Install
the root CA in the system trust store (on Debian/Ubuntu: copy it as a `.crt`
file to `/usr/local/share/ca-certificates/` and run `update-ca-certificates`)
and restart Arexibo. This applies to the player's own requests only, not to
pages loaded by the embedded browser.

## Webpage widgets in "Open Natively" mode

* `[DisplayName]` and `[HardwareKey]` in the URL are replaced with the display's name
  (as assigned by the CMS) and its hardware key, url-encoded.
* A touch or click on the page restarts the widget's duration countdown, so the layout
  doesn't expire or reload while someone is using the page.  This applies to the main
  layout only; keyboard, mouse wheel and dragging don't count.
* Add `[HasFocus]` to the URL (at the very end is cleanest) to make that page take keyboard
  focus once, when it first finishes loading, so a text field the page focuses itself is
  ready for typing without a first touch.  The marker is removed before the page is loaded.
  Without it a native page only gets focus when touched, and the page cannot take it by
  itself (`window.focus()` is not enough).  Meant for one page per layout: with several, the
  last one to finish loading holds focus until a touch moves it.  Only the first load counts.
* The widget's "Background transparent?" option is honored: the native view gets a transparent background, so the layout
  shows through wherever the page paints nothing.  The page itself must be transparent too (e.g. `html, body { background:
  transparent }`).  Unchecked, the view stays opaque white, as before.

## Data Connectors

A Data Connector (a JavaScript file scheduled in the CMS against a DataSet) publishes real-time data that widgets read through
`/realtime`. It follows the official Electron player:

- It runs in a hidden page of its own, for as long as it is scheduled (inside its dates, with its criteria met, the highest priority
  one per DataSet). A connector tied to a geographic area never runs: there is no location source.
- The script is downloaded like any other file and checked against the md5 announced by the CMS before every run; one edited in the
  CMS is downloaded again. A connector that is no longer scheduled stops after three schedule checks, and its data goes with it.
- It has `xiboDC.setData(key, data, {done, error})` (at most one stored write per key every 500 ms, 2 MB a value, 100 keys a connector),
  `notifyHost(key)` (tells the widgets on screen), `makeRequest(url, {type, headers, data, done, error})` (made by the player, no CORS,
  30 s timeout) and `setCriteria(metric, value, ttl)` (a schedule criterion, as from the CMS). The `dataParams` of the event arrive as
  variables of the page, and `onInit()` is called once it is loaded.
- `window.displayTags` is the display's tags in the CMS, as an object (`{name: value}`), set before `onInit()`. The Electron player defines
  `setDisplayTags` but never calls it, so a connector relying on it works here only.
- Widgets read the data at `/realtime?dataKey=...`: JSON, a 404 when there is none (the widget's error callback), a 400 without a key.
- Not supported: geographic areas.

## Display tags in commands

A command (one defined in the CMS and run by code or by a schedule, or one in a shell command widget) can contain `[[name]]`, replaced by
the value of the display's tag of that name when the command runs, so a change in the CMS applies at once. The name is matched exactly
(letters, digits and `_`).

- In an `http|...` or `rs232|...` command each `|`-separated part is filled in on its own, so a value containing `|` cannot move a boundary.
  In a URL (`http://...`) the value is percent-encoded like JavaScript's `encodeURIComponent`; in a JSON part (such as the options of an
  HTTP command, including the body inside them) it is escaped for the string it sits in; elsewhere it is used as typed.
- A shell command is one piece and takes the value as typed: quote the placeholder where the shell needs it (`echo '[[name]]'`). A value
  is trusted as much as the command itself, as both are set in the CMS.
- A tag that is missing, or set without a value, stops the command: it does not run, and is reported to the CMS as failed.
- The list of allowed shell commands is checked on the command as written, before the placeholders are filled in.
- Not supported: the macros of the Electron player (`[EPOCH]`, `[UNIX_TIMESTAMP]`, `[TIMESTAMP]`, `[LAT]`, `[LNG]`); they stay as written.

## Standalone setup with X server

This section is for Linux.  On Windows, the shortcuts made by the installer
do the equivalent: see [Windows](#windows-experimental).

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

anyway if audio or dbus are required, as well other settings for touch screen, better use the provided arexibo.service which runs arexibo-kiosk-start and customize that script.
btw they are automatically installed by the .deb package.

## Useful flags for development

    --debug: verbose logging (SOAP calls, internal state transitions).
    --web-debug: logs every JS console message and page error from every frame of the rendered content, the player's own `arexibo-show:`/`arexibo-shrink:` diagnostics and the JavaScript the player runs on the layout page (region switches, widget reloads, ...) – very verbose, useful when troubleshooting a specific widget that isn’t displaying correctly. Also turns on Chromium's verbose logging and runs it in single-process mode.
    --inspect: shows the web inspector, to debug layout problems.
    --clear: wipes the local file cache (downloaded media/widget pages), forcing a full re-download on next start. Does not affect cached CMS connection settings, it's always recommended to run --clear once after a new git pull or git clone. Cached layouts are also retranslated automatically when a release changes the translator version.

## Some useful environment variables

Normal use:

    AREXIBO_FONT_SCALE: a numeric multiplier (e.g. 0.91) applied globally to font sizes, to correct for rendering differences compared to a reference client on a different platform. Leave unset for the default (no correction).
    QTWEBENGINE_CHROMIUM_FLAGS: standard Qt/Chromium mechanism for passing extra Chromium command-line flags (e.g. GPU-related tuning for a specific graphics driver). arexibo appends its own required flag (--disable-pinch, needed to disable pinch-to-zoom on multitouch panels) to whatever you set here, rather than overwriting it – both apply together.
    QTWEBENGINE_REMOTE_DEBUGGING: QTWEBENGINE_REMOTE_DEBUGGING=9222 as an environment variable lets you inspect the rendered page from another machine’s Chrome/Edge at http://<host>:9222 (via an SSH tunnel if not on the same network) – useful for confirming content renders correctly without needing eyes on the actual totem screen.
    AREXIBO_FORCE_WS_ADDRESS: forces the WebSocket XMR address used for push messages, regardless of what the CMS reports when the player registers (useful when the CMS's own WebSocket XMR eligibility check is unreliable). Leave unset to use the address the CMS provides.
    AREXIBO_SPLASH_MS: minimum time (default 1000) the startup splash screen stays on screen once it has loaded, before the first layout replaces it; 0 disables the minimum. Useful on a fast start from a full cache, where the first layout would otherwise replace the splash almost immediately.
    AREXIBO_LAYOUT_LOAD_TIMEOUT_S: seconds a layout page may go without any load progress before it is stopped and retried (default 20, 0 disables). Widgets only start once the whole page has loaded, so one stuck resource (an iframe, a web font) would otherwise leave the screen blank. The first stop is followed by a reload; if the second attempt also stalls, the page is accepted as it is and the stuck widget stays empty.

Diagnostic only – not for production use:

    AREXIBO_FAKE_USERAGENT: overrides the user agent string reported to the CMS/widgets (e.g. windows to masquerade as a Windows client). Added specifically to investigate a rendering bug that turned out to be unrelated to the user agent at all – misrepresents the player to anything checking it, don’t leave this set normally.
    AREXIBO_STATS_AGGREGATION: `individual` (the default), `hourly` or `daily`: how the Proof of Play records are added up before they are sent (one record per layout and widget and hour or day, sent when that hour or day is over). Overrides the `aggregationLevel` of the CMS display settings, which the CMS only offers to its Android and Windows profiles.
    AREXIBO_SPLASH_INFO: `0` (or `false`, `no`, `off`) hides the line at the bottom of the splash screen with the version, the date and time, the free and total disk space and the time and result of the last collection (read from the local-only `/splash-info`).
    AREXIBO_FAKE_CLIENTTYPE: overrides the clientType reported to the CMS during registration (currently only windows is implemented as an override value; anything else falls back to the real linux type with a warning). Added to test a hypothesis about CMS-side behavior differing by client type – same caveat as above, this misrepresents the player to the CMS’s own registration logic, which can affect other clientType-conditional CMS behavior beyond whatever you’re specifically testing.
