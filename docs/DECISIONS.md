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

## M2 Transcription

- **Engine:** whisper.cpp through `whisper-rs` 0.16, CPU by default; `vulkan` and
  `cuda` features pass through. Beam search (5), `no_context` so chunks do not feed
  each other's mistakes, and the first chunk's detected language is forced for the rest
  of the meeting (switching mid-meeting is rare; flip-flopping on short chunks is not).
- **VAD first:** silence makes Whisper slow and makes it invent words, so only speech
  goes to the model: Silero v5 (the 1 MB ggml build whisper.cpp ships, downloaded with
  the first model) and, when it is missing, a level detector (12 dB over the noise
  floor, clamped to -50..-40 dBFS). Speech is grouped into chunks of up to 28 s,
  joining regions less than 1.5 s apart.
- **Decode at the rate needed:** Opus decodes natively at 16 and 24 kHz, so
  transcription decodes straight to 16 kHz and playback to 24 kHz; nothing holds a
  meeting at 48 kHz in memory.
- **Before speakers:** mic lines are the user (`person_id` = me), Teams lines are
  labelled "Teams" until diarization (M3). `transcript.json` keeps the raw per-track
  output so M3 can re-identify speakers without transcribing again.
- **Models:** the catalog is fixed in `pipeline::models` (Base, Small, Large v3 Turbo q5
  and full) and downloaded from the whisper.cpp repositories on Hugging Face. The
  SHA-256 comes from the hub's `X-Linked-ETag` header (read on the redirect, which is
  where it lives); a mismatch deletes the `.part` file. Small is recommended without a
  GPU, the quantized Large v3 Turbo with one. With no model chosen, the largest
  installed one is used.
- **Jobs:** transcription and downloads run on their own `std::thread`s, not the GPUI
  executor (they block for minutes); the workspace polls them every 250 ms for progress
  and results. One transcription at a time, oldest first.
- **Player:** both tracks mixed into one 24 kHz 16-bit buffer. Speed changes use WSOLA
  (1024-sample Hann frames, half-frame hop, ±10 ms alignment search) so voices keep
  their pitch. Skip silence jumps over quiet stretches of 1.5 s or more, keeping 0.3 s
  either side. Output through `cpal` on its own thread; with no output device the
  same renderer runs against a silent clock, so position and speed still behave.
  Meetings with no audio (the demo) get a plain clock as long as the meeting.
- **Following playback:** the transcript's rows sit directly in the scroll container so
  the active row can be scrolled into view by index; it only follows while playing, so
  it never fights the user's own scrolling.

## No console windows (Windows)

- Release builds use the GUI subsystem, so the app itself never opens a console (debug
  builds keep one for logs). A GUI-subsystem app that starts a console program (the
  coding CLI in M4, `git`, `explorer`) gets a new console window per spawn unless the
  spawn passes `CREATE_NO_WINDOW`. All spawns go through
  `switchscribe_agent::process::command`, and `clippy.toml` disallows
  `std::process::Command::new` so a direct spawn fails CI. Async runners (M4) wrap the
  same `Command` (`tokio::process::Command::from`) rather than building their own.
