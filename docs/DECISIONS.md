# Decisions

Non-obvious calls, newest last. Append when you make one.

## M0

- **GPUI stack:** GPUI Kit 0.7.3 on the application-patched GPUI Fast stack in `vendor/`,
  copied unchanged from switchhead (which took it from AgentOpsChat), with the same
  `[patch]` entries and `Cargo.lock` as the starting point. One stack across the suite
  means Switchyard and switchhead components port over without changes. Update
  `vendor/` from switchhead rather than editing it here.
- **Fonts:** Geist and Geist Mono (OFL, bundled), the suite's fonts. The prototype was
  drawn in IBM Plex Sans / Mono; sizes and weights follow the prototype, the family
  follows the suite. Switching is one constant in `theme.rs` plus the font files.
- **Tokens:** the prototype's colors live in `crates/app/src/theme.rs` (`Palette::dark`).
  Red is reserved for recording; warnings and errors use amber.
- **Speaker colors:** 8 colors at oklch ~0.74 / 0.12. The prototype defines five (hues
  250, 150, 300, 340, 110); hues 200, 55 and 275 are provisional.
- **Store:** `rusqlite` with the `bundled` feature (SQLite compiled in, FTS5 enabled), so
  Windows needs no system SQLite. Schema changes are numbered migrations in
  `crates/store/src/db.rs`, tracked in `PRAGMA user_version`; a newer schema refuses to
  open rather than being downgraded. WAL mode so the M4 MCP server can read while the
  app writes.
- **Search input:** the store treats a query as plain words, each a prefix match
  (`"cut"* "over"*`), never as FTS5 syntax, so nothing a user types is a syntax error.
- **Data folder:** `%APPDATA%\switchscribe` on Windows (the platform data dir plus
  `switchscribe` elsewhere); `SWITCHSCRIBE_DATA` overrides it.
- **Settings file:** `settings.json` in the data folder, every field defaulted, unknown
  fields ignored, written atomically (temp file + rename) off the UI thread. A malformed
  file loads as defaults so the app always starts.
- **Database failure:** if `switchscribe.db` cannot be opened, the app still starts on an
  empty in-memory index and shows an amber banner saying nothing is being saved.
- **Demo data:** `--demo` / `SWITCHSCRIBE_DEMO=1` loads the prototype's meetings,
  people and transcript into an in-memory store (dated relative to today) plus the
  summary, action items, notes and Ask exchange later milestones will produce. It never
  touches the data folder. Without it the app opens on the real, empty store.

## M1 Capture

- **WASAPI for both tracks, through the `wasapi` crate.** The build plan listed `cpal`
  for the mic; `wasapi` already covers process loopback, system loopback and capture
  with one event-driven read loop, and lets the mic open on the default
  *communications* device (what Teams uses), which `cpal` does not expose. One audio
  stack, fewer dependencies.
- **Teams' process tree:** the root `ms-teams.exe` (new Teams) or `Teams.exe` (classic)
  whose parent is not Teams, captured with include-tree loopback so the WebView2
  children that render call audio are included. Not found, or loopback refused (older
  than Windows 10 20348): whole-system loopback on the default playback device, with an
  amber banner saying so.
- **Track format:** mono Opus at 48 kHz, 32 kbit/s VoIP mode, in Ogg (RFC 7845), about
  14 MB per track-hour. Sources deliver 48 kHz float (shared mode with autoconvert);
  anything else is downmixed and linearly resampled. Pages close once a second and are
  flushed, so a crash loses at most a second; the final granule position trims the
  encoder padding, so decoded length equals recorded length.
- **One clock for both tracks.** Loopback delivers nothing while nobody talks, so a
  track that has been quiet for 200 ms is padded with silence up to the recording
  clock. Pausing stops the clock and drops audio, so files skip the paused stretch and
  bookmarks stay aligned with them.
- **A lost device does not stop the recording.** Each track has its own thread; a mic
  unplugged mid-call ends only that track and shows "Microphone disconnected at 12:04.
  Recording continues on the Teams track." (amber, never red).
- **Meeting folders:** id `YYYYMMDD-HHMMSS` (sorts by time); `mic.opus`, `remote.opus`,
  `capture.json` (sources, per-track length and errors, bookmarks) and `notes.md`. The
  meeting row is `recording` while capturing and `processing` once stopped; M2 picks up
  `processing` meetings. A row left `recording` by a crash is moved to `processing` at
  startup.
- **Mini recorder:** a separate borderless always-on-top window (GPUI `PopUp`), so it
  stays visible over Teams; dragging anywhere on the pill moves it. It reads the
  workspace's session on each 100 ms tick rather than holding its own state.
- **Notes:** the live view's notes are a plain Markdown textarea saved to `notes.md`
  600 ms after typing stops. The mini recorder's pencil starts a `- [mm:ss] ` line;
  `[mm:ss]` anchors render as links to the moment in the Notes tab.
- **Off the UI thread:** creating the folder, opening devices and the index write run on
  the background executor; stopping (joining the capture threads, finishing the files)
  too. Quitting mid-call is the one exception: it finishes the files synchronously so
  they end cleanly.
- **COM apartments:** device listing and capture threads join COM's multithreaded
  apartment on threads of their own, never on the UI thread, where GPUI sets up OLE.
- **`SWITCHSCRIBE_FAKE_AUDIO=1`** records two synthetic voices on any OS (and the demo
  always does, into a temporary folder), so the whole flow runs and can be screenshotted
  without Windows audio. Elsewhere, Record on Linux or macOS shows an amber banner.
- **Tray icon** (Windows only, `tray-icon`): a ring or disc drawn in code per state
  (gray idle, amber armed, red recording, red ring paused, teal processing) with Start /
  Stop recording, Show and Quit; menu events are polled every 200 ms.

