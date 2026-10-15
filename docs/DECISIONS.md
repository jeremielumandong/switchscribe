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

## M3 Speakers

- **sherpa-onnx, shared libraries:** pyannote segmentation 3.0 plus NeMo TitaNet-small
  embeddings through `sherpa-onnx` 1.13 with its `shared` feature. Linked statically,
  onnxruntime and whisper.cpp each carry their own copy of parts of the C++ standard
  library and the process crashes in `std::regex` (`munmap_chunk(): invalid pointer`);
  on Windows the static build is also `/MT`, which mismatches the rest. So
  `sherpa-onnx-c-api` and `onnxruntime` ship as DLLs / `.so` files next to the
  executable (Linux finds them through an `$ORIGIN` rpath set in the app's `build.rs`).
- **Embeddings and thresholds:** TitaNet-small (192 dims, 40 MB) separated speakers
  best on sherpa-onnx's four-speaker sample: one speaker's turns scored 0.51–0.74
  against each other, different speakers at most 0.47. So a match is high at 0.6 and
  medium at 0.45, and diarization clusters at 0.9 (it found the four speakers; 0.5
  found eight). Samples use up to 30 s of a speaker's longest turns.
- **Matching:** one-to-one, strongest pairs first. Attendees win at medium; anyone else
  only at high, so a colleague who was not invited is named only when the voice is
  unmistakable. The user's voiceprint is left out of the Teams-track library: their
  voice there is echo, and naming it would hide a real speaker.
- **Mic is the user:** only the Teams track is diarized. The mic track's sample feeds
  the user's voiceprint after every meeting, which is how they are "enrolled" until
  onboarding (M6) records a dedicated sample.
- **Voiceprints at rest:** sealed with DPAPI (`CryptProtectData`, current user) on
  Windows; elsewhere stored plain, marked by a one-byte prefix so a blob says how to
  open it. Rows that will not open (another user, another machine) are skipped, not
  errors.
- **Naming:** naming a speaker updates every line with that label, adds them to the
  meeting's attendees, folds the cluster's sample into their voiceprint when Remember
  is on (a running mean), and rewrites `speakers.json`. A meeting is `needs-labels`
  while any cluster is unconfirmed. "Later" only hides the card for this session.
- **Failure is soft:** if identification fails (models missing or broken), the
  transcript stays with its Teams lines unnamed; transcription never fails because of
  speakers.

## M4 Agent

- **Two ways to talk to the CLI.** Summaries put the transcript in the prompt and read
  one JSON object back from stdout: one call, no tools, and it works the same with every
  CLI. Ask goes through the MCP server instead (`switchscribe mcp --ask`, passed to the
  CLI per run), because a question may need any meeting and the CLI decides what to
  read. Both run headless with the prompt on stdin, so nothing long or quoted goes on a
  Windows command line.
- **Per-CLI flags.** Claude: `-p --output-format text`, plus `--mcp-config <json>
  --strict-mcp-config --allowedTools mcp__switchscribe` so it sees only our server.
  Codex: `exec --skip-git-repo-check --output-last-message <file> -c
  mcp_servers.switchscribe.*` and `-` for stdin. Gemini has no per-run MCP flag, so
  the run gets its own working folder with `.gemini/settings.json` (trusted). Codex and
  Gemini were checked against their docs, not run; Claude Code was run end to end.
- **Finding the CLI:** a PATH search that honours PATHEXT, since npm installs `claude`
  as `claude.cmd` on Windows. Spawns go through `process::command` (no console).
- **Writes wait for the user.** In ask mode `update_summary` and `create_jira_issue`
  only add a row to `pending_actions`; the Ask panel shows it as a card with what would
  change, and Approve applies it in the app's process. `switchscribe mcp` without
  `--ask` (the user's own CLI) applies `update_summary` directly but still queues Jira.
- **Citations** come back as `[[meeting_id@mm:ss]]`, which the panel turns into chips;
  a malformed one stays as text. Summary lines carry `at` as the transcript's own
  `mm:ss`, clamped to the meeting.
- **Owners** match People by full name, else by a first name only one person has.
- **Re-runs keep status.** Action items whose text is unchanged keep Done.
- **The user is "you".** The Ask prompt names the user as transcripts show them, so
  "what did I commit to" works.

## No console windows (Windows)

- Release builds use the GUI subsystem, so the app itself never opens a console (debug
  builds keep one for logs). A GUI-subsystem app that starts a console program (the
  coding CLI in M4, `git`, `explorer`) gets a new console window per spawn unless the
  spawn passes `CREATE_NO_WINDOW`. All spawns go through
  `switchscribe_agent::process::command`, and `clippy.toml` disallows
  `std::process::Command::new` so a direct spawn fails CI. Async runners (M4) wrap the
  same `Command` (`tokio::process::Command::from`) rather than building their own.

## M5 integrations (library)

- **Graph sign-in is the OAuth device code flow.** switchscribe is a public client: it
  cannot keep a client secret, and a localhost redirect listener is fragile on locked-down
  work laptops (firewall prompts, blocked ports). With device code the app shows a short
  code, the user signs in at `microsoft.com/devicelogin` in any browser (MFA and
  conditional access included), and the app polls the token endpoint every `interval`
  seconds (`slow_down` adds 5). Scopes: `offline_access Calendars.Read User.Read`.
- **The client id is the user's own app registration.** We ship no multi-tenant app id:
  most tenants need admin consent for a third-party app, while a user (or their IT) can
  register a single-tenant public client ("Allow public client flows" on, delegated
  `Calendars.Read` + `User.Read`) and paste its id into Settings. Tenant defaults to
  `organizations`. Tokens are sealed in the store's `secrets` table (DPAPI on Windows),
  never in `settings.json`; only the client id, tenant and account name are settings.
- **ICS is the fallback** (risk table: Graph needs admin consent). An Outlook "Publish
  calendar" link (`webcal://` mapped to `https://`) is fetched and parsed by a small
  hand-written parser: unfolding, text unescaping, `CN=` / `mailto:` attendees (rooms and
  resources dropped), `X-MICROSOFT-SKYPETEAMSMEETINGURL` or a `teams.microsoft.com` link
  marks a Teams meeting. `TZID` times are read as the machine's local time (Outlook writes
  Windows zone names; mapping them is not worth it for one's own calendar). **`RRULE` is
  not expanded:** a recurring series appears once, at its first instance (moved instances
  with `RECURRENCE-ID` appear on their own). Graph's `calendarView` expands recurrences,
  so this only limits the fallback.
- **Matching a meeting to an event:** in progress at the recording's start, or starting
  within 10 minutes after it; Teams meetings win, then the closest start. All-day and
  cancelled events are ignored.
- **Jira descriptions are ADF** (REST v3 requires Atlassian Document Format): the item
  text, Owner, Due as said (plus the parsed date), "From <meeting> on <date> at mm:ss",
  the transcript line as a blockquote, and the link back as a paragraph whose text is
  the URL itself, so it stays readable if Jira strips a non-http link mark. Empty parts
  are omitted (ADF rejects empty text nodes). When Jira refuses only `duedate` or
  `assignee` (not on the create screen, or an unassignable user) the issue is created
  again without them. Assignees are set only for an unambiguous user search (one active
  human match, or one exact display-name match).
- **Due dates** are parsed against the meeting's local day: weekday names (and "next" /
  "this" weekday) mean the next such day strictly after the meeting; "today"/"EOD",
  "tomorrow"; "end of week" is that week's Friday; "Oct 22" / "22nd of October" (rolled
  into next year when already past, unless a year is given); ISO dates. Anything else
  ("next sprint", "soon") sets no `duedate` and stays as text in the description.
- **.docx is written by hand:** five parts (`[Content_Types].xml`, `_rels/.rels`,
  `word/document.xml`, its rels and a small `styles.xml` with Title / Heading 1 / List
  Paragraph) zipped with the `zip` crate. The content is headings and paragraphs, so a
  generator crate would add weight for nothing; all text is XML-escaped and characters
  XML 1.0 cannot hold are dropped.
- **The JSON export is versioned** (`"format": "switchscribe.meeting/1"`); fields are only
  added. Times are `*_ms` from the meeting start plus `at` as `mm:ss`.

## M5 detection and integrations (app)

- **A call is a Teams capture session.** New Teams opens a mic stream when a call starts
  and keeps it open while muted, so an active WASAPI *capture* session owned by a
  process in the Teams tree (`ms-teams.exe` and its WebView2 children) is the call.
  Render sessions were rejected: Teams plays notification sounds outside calls. Window
  titles (`EnumWindows`) only name it: "<meeting> | Microsoft Teams", with Teams' own
  views (Chat, Calendar, ...) ignored.
- **Hysteresis:** a call counts after two polls (2 s apart) and ends after four missing
  polls, so a headset switch does not split a meeting.
- **Auto-stop** fires 30 s after the call ends, or when the Teams track has been under
  2% level for the set minutes (default 3, Never possible). Recordings started from
  Settings' test button never stop on their own. The reason shows in the notice strip.
- **"Always record" is per title,** matched exactly (case-insensitive); a title is the
  calendar subject when one matches, else the window's meeting name.
- **Single instance over loopback TCP,** not a named pipe or mutex: one portable piece of
  code, and the same channel carries `switchscribe://` links. `instance.port` holds the
  port and a random key; requests without the key are dropped. The scheme is registered
  per user under `HKCU\Software\Classes` with `reg.exe` (through `process::command`, no
  window) at each start, so moving the exe keeps links working.
- **Notices are one strip above the content,** replaced by the next one: export results
  (with Show in folder), Jira results (with View issue) and setup hints. Warnings are
  amber; nothing here is red.
- **The calendar refreshes every 10 minutes** on the background executor (and when its
  settings change). Invitees in the library become the meeting's attendees; the rest are
  offered on the Name-a-speaker card as dashed "invited" chips, and naming a speaker with
  one adds them like a typed name.
- **Exports go through the platform save dialog;** where there is none (Linux without an
  XDG portal) they land in Downloads, else `<data>/exports`.
- **`export_meeting` over MCP** returns Markdown, SRT, VTT or JSON as text (Word is
  binary) and leaves out the user's notes, which live in the meeting folder the server
  does not read. This is the one reason `agent` depends on `integrations`.
- **Approved `create_jira_issue` proposals are created by the app,** which holds the Jira
  token; `mcp::apply_pending` refuses them.
