# Plan

Milestones from the [build plan](docs/BUILD-PLAN.md). Each ends in something usable on a
real Teams call; build them one at a time.

## M0 Skeleton ✓

Done when the app opens with an empty meetings list.

- [x] Cargo workspace with the seven crates (app, capture, pipeline, speakers, store,
      agent, integrations) on the vendored GPUI Fast stack shared with switchhead.
- [x] Store: data folder layout, SQLite index with migrations (meetings, people,
      segments, FTS5 over segment text), prefix-word search.
- [x] Settings file: `settings.json`, defaults for every field, atomic writes off the UI
      thread.
- [x] GPUI shell laid out like the prototype (`docs/design/switchscribe.dc.html`): title
      bar (search, Record, CLI selector, Ask toggle, settings), meetings sidebar grouped
      Today / Yesterday / This week / Older with status icons and attendee avatars,
      secondary nav, main pane, collapsible Ask panel (Ctrl+J) and sidebar (Ctrl+B).
- [x] Meeting detail: header, processing stepper, Summary / Transcript / Action items /
      Notes tabs; timestamp chips cue the transcript.
- [x] Search screen on the store's FTS5 index; People and Settings screens read the
      store and settings file.
- [x] Empty state (no meetings) and an amber banner if the database cannot be opened.
- [x] `--demo` loads the prototype's data into an in-memory store.

## M1 Capture ✓ (needs a real-call check on Windows)

Teams process loopback plus mic to two Opus files, device picker, mini recorder, tray
icon. Done when a 30-minute Teams call records both tracks cleanly.

- [x] Capture crate: WASAPI process loopback of Teams' tree (include-tree), whole-system
      loopback fallback, mic on the default communications device or the chosen one.
- [x] Mono 48 kHz Opus in Ogg per track, flushed every second, decoder for M2.
- [x] Recorder: a thread per track, one clock (silence padded), pause, bookmarks, live
      levels, notices; a lost track keeps the other recording; `capture.json`.
- [x] Record / Stop (Ctrl+Shift+R, title bar, empty state), meeting row and folder,
      `recording` → `processing`, crash recovery at startup.
- [x] Live view: header with elapsed time, Pause and Stop, track meters, notes editor
      saved to `notes.md`, bookmarks (Ctrl+Shift+B).
- [x] Mini recorder: always-on-top pill with pulsing dot, time, You / Teams meters,
      Bookmark, Quick note, Pause, Stop.
- [x] Settings: capture mode and microphone picker, saved to `settings.json`.
- [x] Tray icon mirroring the recorder, with Start / Stop, Show, Quit (Windows).
- [x] Amber banners for fallbacks and lost devices; `SWITCHSCRIBE_FAKE_AUDIO=1`.
- [ ] Real-machine check: a 30-minute Teams call on Windows 11 records both tracks.

## M2 Transcription

VAD, post-call Whisper per track, merged transcript, audio player synced to segments,
model manager.

- [x] `SpeechEngine` on whisper.cpp (whisper-rs): beam search, language detected once
      per meeting and reused, attendee names and title as the prompt, noise markers
      dropped.
- [x] VAD: Silero (ggml, through whisper.cpp) with a level-based fallback; speech is
      grouped into chunks of up to 28 s so silence never reaches the model.
- [x] Per-track transcription merged into one transcript; `transcript.json` kept in the
      meeting folder; segments stored and indexed; the meeting turns `ready`.
- [x] Processing queue on its own thread: starts when a recording stops and at startup
      for meetings left `processing`; the stepper shows real progress.
- [x] Model manager in Settings: Base, Small, Large v3 Turbo (q5 and full); download
      with progress, SHA-256 check and cancel; Use / Remove; GPU switch; an amber
      banner when no model is installed.
- [x] Audio player: both tracks mixed, play / pause, 1× 1.25× 1.5× 2× without pitch
      change (WSOLA), skip silence, speaker lanes with the playhead, click to seek,
      the transcript line follows and scrolls; Play on the active line.
- [ ] Real-machine check: download a model and transcribe a recorded Teams call on
      Windows; play it back through the speakers at each speed.

## M3 Speakers

Diarization on the Teams track, Name-a-speaker card, People screen, voiceprint matching.

- [x] Diarization of the Teams track with sherpa-onnx (pyannote segmentation 3.0 and
      NeMo TitaNet-small embeddings), after transcription; transcript lines take the
      speaker they overlap most; `speakers.json` keeps each cluster's sample and clips.
- [x] Voiceprint matching: one-to-one, attendees first, non-attendees only on a high
      score; high confidence names the speaker, medium suggests ("?" badge).
- [x] Voiceprints sealed with DPAPI on Windows (store migration 2); the user's own
      voiceprint grows from the mic track after every meeting; the user is a person
      from the first start.
- [x] Name-a-speaker card: clips to play, suggestions ranked by voice with confidence,
      a typed name, Remember this voice, Later; Relabel on a Teams line reopens it.
- [x] People screen: meetings heard, samples, last heard; Play sample, Merge, Forget
      voice.
- [x] Settings → Models: the speaker models download with the first transcription
      model or on their own; the stepper shows Identify speakers.
- [ ] Real-machine check: a Teams call with three or more people; name one speaker,
      record another call with them and see them named automatically.

## M4 Agent

MCP server, CLI runner, summary templates, Summary and Action items tabs, Ask panel, FTS
search filters.

- [x] CLI runner: Claude Code, Codex and Gemini CLI found on PATH (PATHEXT, so `.cmd`
      shims count), run headless with the prompt on stdin, no console window, a
      timeout and Stop; the CLI's own sign-in, no API keys.
- [x] Summaries after every transcription (Settings switch) and on Re-run summary:
      five templates picked from the title and editable in Settings; the CLI returns
      JSON whose every line carries a time; owners matched to People; the stepper
      shows Summarize. Store migration 3: summaries, action items, pending actions.
- [x] Summary and Action items tabs from the store; status Open/Done (re-runs keep the
      status of items they keep); the cross-meeting Action items screen with Show done.
- [x] MCP server, `switchscribe mcp [--ask]` over stdio: list_meetings, get_transcript,
      get_summary, search_meetings, get_action_items, update_summary,
      create_jira_issue. Writes from the Ask panel wait as approve/deny cards.
- [x] Ask panel: this meeting or all meetings, the CLI reads through the MCP server,
      answers carry timestamp chips that open the moment, Stop, follow-ups keep the
      conversation; Settings shows the `claude mcp add` line for the user's own CLI.
- [x] Search filters: person and Past week / Past month.
- [ ] Real-machine check: on Windows with Claude Code installed through npm
      (`claude.cmd`), a call summarizes with no console window flashing; Ask answers
      with chips; Codex and Gemini CLI flags against their current releases.

## M5 Detection and integrations

Teams auto-detect prompt, Graph calendar, Jira export, Markdown/Word/SRT export.

- [x] Meeting detection: an active Teams capture session (the mic stream Teams holds
      open for the whole call, muted or not) in the Teams process tree, polled every
      2 s with hysteresis; the meeting window's title names it. Off Windows,
      `SWITCHSCRIBE_FAKE_MEETING` fakes a call.
- [x] Record prompt in the bottom-right corner (Record / Ignore / Always record this
      title), or record straight away when Settings says so; auto-stop 30 s after the
      call ends or after N silent minutes; test recordings never stop on their own.
- [x] Outlook calendar: Microsoft Graph with the user's own Entra app (device code
      sign-in, refresh, `calendarView` every 10 min) or a published ICS link. A
      recording takes its event's subject as its title; invitees in the library become
      attendees, the rest appear as "invited" suggestions on the Name-a-speaker card.
- [x] Jira Cloud: settings with a sealed API token and a connection test; Send one item,
      tick several on the Action items screen, or approve the agent's
      `create_jira_issue`. Issues quote the transcript line, link back with
      `switchscribe://meeting/<id>?t=<s>`, take the owner as assignee and a parsed due
      date; the item then shows View KEY.
- [x] Exports from the meeting's Export menu: Markdown, Word, SRT, VTT and JSON, to a
      save dialog (or Downloads when the platform has none); the notice offers Show in
      folder. `export_meeting` over MCP returns the text formats.
- [x] One app per user: a second launch or a clicked `switchscribe://` link hands over
      to the running app and opens the moment; Windows registers the scheme per user.
- [ ] Real-machine check: join a Teams call on Windows and see the prompt within a few
      seconds, named from the meeting window or calendar; leave and see the recording
      stop 30 s later; a muted call is still detected.
- [ ] Real-machine check: Graph sign-in against a work tenant (or the ICS fallback);
      send two action items to a real Jira Cloud project and click the link in the
      issue; open each export in Word and a video player (SRT/VTT).

## M6 Live and polish

Live captions, onboarding, error states, signed installer and auto-update.

- [x] Live captions (Settings → Models, also from the live view mid-call): each track cut
      at pauses and captioned by Base or Small with greedy decoding; shown in the live
      view, newest line lighter, and in the mini recorder, which grows on hover.
- [x] First-run setup: welcome and what stays local, audio with a live meter test, the
      model for this machine, your name and a headset tip, calendar and Jira, the consent
      notice; skippable, and Settings → Run setup again.
- [x] Error states: a missing coding CLI turns the title-bar dot amber with the reason;
      captions without a model warn and keep recording (mic lost, Teams fallback, missing
      model and the empty state were already in).
- [x] Installer: Inno Setup, per user, no admin; Start with Windows (`--background`
      opens to the tray); `switchscribe://` registered; uninstall keeps the data folder.
- [x] Release workflow on `v*` tags: release build, optional signing (PFX or Azure
      Trusted Signing), installer plus `.sha256`, GitHub release. (Replaced by the
      three-platform pipeline below.)
- [x] Updates: a check every six hours (Settings → Updates, or off), Update button in the
      title bar, download with SHA-256 check, silent install and restart.
- [x] Keep audio: Forever / 7 / 30 / 90 days, enforced for finished meetings at start.
- [x] Release pipeline for Windows, macOS and Linux, the same as Switchyard's (see
      `packaging/README.md`): Bump version PR, three release workflows on `main` into one
      draft release; Inno installer, universal DMG, AppImage.
- [ ] Real-machine check: cut `v0.1.0` (Bump version, then the three release workflows,
      then publish the draft) and install on a fresh Windows machine; setup through to
      the first recorded meeting without help.
- [ ] Real-machine check: open the macOS DMG and the Linux AppImage on a clean machine;
      the demo and transcription of an imported meeting work.
- [ ] Real-machine check: live captions keep up on a laptop CPU with Base during a real
      call; the mini recorder grows on hover.
- [ ] Real-machine check: install `v0.1.1` over it from the Update button; the app
      restarts on the new version with its data intact; Start with Windows opens to the
      tray.

## Follow-ups

- Virtualize the sidebar and transcript lists (`uniform_list`) before real meeting
  counts grow; M0 renders them as plain scroll views.
- Command palette (Ctrl+K) from switchhead's `palette.rs`.
- Port switchhead's rotating log file and crash reports.
- Windows icon resource (switchhead's `build.rs`): the icon now exists
  (`packaging/icons/switchscribe.ico`); the exe itself still has none.
- Global hotkeys (Ctrl+Shift+R / Ctrl+Shift+B work only while a switchscribe window
  has focus); `global-hotkey` would make them system-wide.
- Mini recorder: snap to screen edges.
- Settings: a test recording with playback (onboarding has a meter test only).
- Verify the update installer's Authenticode signature before running it
  (`WinVerifyTrust`, signer matching the running exe) and refuse unsigned updates once
  releases are signed.
- Decide whether a GPU (Vulkan) build ships alongside the CPU one.
- Live captions: a vocabulary prompt (glossary, attendees) and showing who on the Teams
  track speaks (live diarization).
- A "Delete everything" button in Settings (Storage & privacy).
- Echo bleed: match mic segments against the loopback track (M3 leaves the user's
  own voice out of the Teams-track library, which hides most of it).
- Re-identify speakers in meetings transcribed before the speaker models were
  installed (`transcript.json` is kept for this), and a Re-enroll action on People.
- Delete a person outright (People only forgets the voice today).
- GPU builds: a `vulkan` (Windows/Linux) and `cuda` feature pass through to whisper-rs;
  the release pipeline (M6) decides which build ships.
- A Retry action for a failed transcription (today it retries at the next start).
- Drag the playhead (click-to-seek only today) and keyboard shortcuts for the player
  (space, arrows).
- Stop and timeouts kill the CLI process, but a `.cmd` shim's child (node) can outlive
  it on Windows; a job object would take the whole tree.
- `--demo` in the Ask panel runs a second, separate demo store, so changes the agent
  proposes there never reach the app's cards (the demo seeds one card instead).
- Jira Server / Data Center (personal access tokens, REST v2) if anyone needs it; M5
  speaks Jira Cloud only.
- Expand `RRULE` in ICS feeds (a recurring series shows only its first instance today;
  Graph is unaffected).
- Detection for Teams in a browser and for Zoom (after v1, with PipeWire capture).
- Pending `create_jira_issue` proposals from the user's own CLI session (`switchscribe
  mcp --ask` in a terminal) show only when the Ask panel next loads its cards.
- Summaries for long meetings: the whole transcript goes in one prompt; split past the
  CLI's context if three-hour calls show up.
- Large meetings: the player holds 24 kHz 16-bit audio in memory (about 170 MB an
  hour); stream from the Opus files if long recordings make that a problem.
- Ship sherpa-onnx's and onnxruntime's license notices with the packages (Geist's and
  switchscribe's are bundled).
- Capture on macOS (ScreenCaptureKit / Core Audio taps) and Linux (PipeWire); the release
  packages build there today without it.
