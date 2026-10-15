# switchscribe — Build Plan

Oct 15, 2026 · @Jeremie

## Overview and goals

switchscribe is a local-first meeting recorder and transcriber in Rust/GPUI. It captures Teams calls on Windows without a bot joining, transcribes on-device, names who spoke, and turns each meeting into searchable notes and action items. It joins switchhead, Switchyard and Snapwright, reusing their UI components and coding-CLI integration.

Goals:

- Record any Teams meeting on Windows with one click, no bot or admin consent.
- Keep audio and transcripts on the machine; no cloud speech service.
- Label speakers automatically, by name after a one-time enrollment.
- Produce a summary, decisions and action items for every meeting.
- Let a coding CLI (Claude Code, Codex, Gemini CLI) query and act on all past meetings.

v1 targets (proposed, adjust after first benchmarks):

- A 60-minute call is fully transcribed within 5 minutes of hanging up on a GPU machine.
- Your own speech is attributed to you in every segment.
- A returning, enrolled colleague is named correctly without manual relabeling in most meetings.
- Search across 100+ meetings returns results instantly.

## Scope

v1 is Windows-only and Teams-first; everything else is designed for but not built.

| Area | v1 | Later | Not planned |
| --- | --- | --- | --- |
| Platforms | Windows 11 | Linux (Omarchy, PipeWire), macOS | Mobile |
| Meeting apps | Teams (desktop) | Zoom, Google Meet in browser, any app by process | Joining calls as a bot |
| Capture | Teams audio + your mic, separate tracks | Screen snapshots via Snapwright | Video recording (Snapwright owns it) |
| Transcription | Local Whisper, after the call, plus live captions | Live translation | Cloud speech APIs |
| Speakers | You from the mic track; others by voiceprint | Shared team voiceprint library | Voice cloning |
| Intelligence | Summary, decisions, action items via the coding CLI | Cross-meeting topic tracking | Built-in hosted LLM |
| Integrations | Outlook calendar (Graph), Jira, Markdown export | switchhead PR links, Slack/Teams post | Teams compliance recording |

## Architecture

Seven crates in one Cargo workspace turn two local audio tracks into a meeting that both the app and your coding CLI can read.

```text
 Teams audio ─┐                                   ┌─> app (GPUI) ── you
              ├─> capture ─> pipeline ─> store ───┤
 Your mic ────┘    (Opus)    (VAD, ASR,  (SQLite, └─> agent (MCP) ── coding CLI
                             diarize)     FTS5)          │
                   speakers <────┘                 integrations (Graph, Jira, export)
```

Audio, models and the store are all local; the coding CLI reaches meetings only through switchscribe's MCP server.

| Crate | Job | Key dependencies |
| --- | --- | --- |
| app | GPUI shell, screens, mini recorder, tray icon | gpui, Switchyard UI components |
| capture | Teams process loopback, mic, meeting detection | windows, wasapi, cpal |
| pipeline | VAD, transcription, diarization, track merge | whisper-rs, sherpa-onnx or polyvoice |
| speakers | Voiceprint library and matching | sherpa-onnx embeddings |
| store | Meeting folders, SQLite + FTS5, settings | rusqlite |
| agent | MCP server, CLI runner, summary templates | Switchyard's MCP and CLI code |
| integrations | Graph calendar, Jira, exports | reqwest, oauth2 |

## Audio capture (Windows, Teams)

Capture Teams and your mic as two separate tracks; the split is what makes "who is speaking" reliable later.

- **Teams audio:** WASAPI process loopback (`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` via `ActivateAudioInterfaceAsync`) in include-tree mode, so Teams and its child processes are captured and nothing else. Microsoft's sample for this API requires Windows 10 build 20348 or later, so target Windows 11.
- **Fallback:** classic system-wide loopback on the default playback device when process capture fails or the PID can't be found.
- **Your mic:** WASAPI capture on the default communications device, with a device picker in Settings to match whatever Teams uses.
- **Format:** record both tracks at the device rate (usually 48 kHz) and store them as Opus; resample to 16 kHz mono only for the speech models.
- **Echo bleed:** on speakers, the mic also hears remote voices. Use the loopback track as a reference: when a mic segment closely matches what played through Teams, attribute it to the remote track. Recommend a headset in onboarding.
- **Meeting detection:** watch audio sessions (`IAudioSessionManager2`) for an active Teams session, cross-check the window title and the calendar, then show a "Record this meeting?" toast. Auto-stop after the Teams session goes silent for a few minutes.
- **Crates:** `windows` (windows-rs) for the COM/WASAPI calls, `wasapi` for convenience wrappers, `cpal` for the mic.
- **Later on Linux:** PipeWire per-application node capture gives the same two-track model on Omarchy.

## Transcription

Transcribe each track separately with local Whisper, then merge by timestamp; live captions use a small model and are replaced by a high-accuracy pass after the call.

1. **VAD:** Silero VAD splits each track into speech segments and skips silence.
2. **Post-call pass:** `whisper-rs` (whisper.cpp bindings) with a large turbo-class model, CUDA or Vulkan when a GPU exists, CPU otherwise. Word-level timestamps on.
3. **Merge:** interleave both tracks' segments by start time into one transcript.
4. **Live captions (optional):** 5–10 second chunks through a small model while the call runs; marked provisional and overwritten by step 2.

Accuracy:

- **Vocabulary prompt:** feed a glossary (EZBook, AKS, Cerberus, product names) plus attendee names from the calendar as Whisper's initial prompt.
- **Language:** auto-detect per meeting, overridable.
- **Engine trait:** put ASR behind a `SpeechEngine` trait so sherpa-onnx models can be swapped in without touching the pipeline.
- **Model manager:** download, verify and switch models from Settings; show size and expected speed per model.

## Speaker recognition

You are identified by track; everyone else is diarized on the Teams track and matched against a local voiceprint library.

- **You:** every mic-track segment is you. Your own voiceprint is enrolled during onboarding and used to reject echo.
- **Diarization:** segmentation + speaker embeddings + clustering on the Teams track, producing anonymous clusters (Speaker 1, 2, …). Use the official `sherpa-onnx` crate, which wraps its offline diarizer and embedding extractor; `polyvoice` (pure Rust, ONNX via `ort`) is the alternative. The older `sherpa-rs` bindings are reported archived, so avoid them. Keep this behind a `Diarizer` trait.
- **Matching:** average each cluster's embeddings and compare to stored voiceprints by cosine similarity, restricted to calendar attendees first.
  - High similarity: auto-name the cluster.
  - Middle band: suggest a name and ask to confirm.
  - Low: leave as "Speaker N" and ask once per meeting.
- **Enrollment:** confirming a name saves that cluster's centroid; each later confirmed meeting refines it (rolling average, capped sample count).
- **Relabel:** renaming a speaker applies to every segment in that cluster and updates the voiceprint.
- **Limits:** overlapping speech and several similar voices will mislabel some segments; the transcript view makes fixing them one click.

## Summaries, search and the coding-CLI agent

switchscribe has no built-in LLM; like Switchyard, it drives the coding CLI you choose through an MCP server it hosts.

- **MCP tools exposed:** `list_meetings`, `get_transcript`, `search_meetings`, `get_action_items`, `update_summary`, `create_jira_issue`, `export_meeting`.
- **After every call:** run the CLI with a summary template to produce the summary, decisions, action items (owner, due date if said) and open questions, each linked to transcript timestamps.
- **Templates:** standup, 1:1, planning, vendor call and custom; picked automatically from the calendar title, editable in Settings.
- **Ask panel:** chat scoped to this meeting or all meetings ("What did we decide about the AKS migration last week?"); answers cite meeting and timestamp, and a click jumps to the audio.
- **Search:** SQLite FTS5 over transcript segments, filterable by person, date and meeting; semantic search with local embeddings is a later add.
- **CLI choice:** Claude Code, Codex or Gemini CLI, reusing Switchyard's CLI runner and selector.

## Integrations

Calendar and Jira ship in v1; the suite links come once the core works.

| Integration | What it does | How | Phase |
| --- | --- | --- | --- |
| Outlook calendar | Names the meeting, pre-arms recording, lists attendees as speaker candidates, picks the summary template | Microsoft Graph, delegated OAuth, calendar read scope | v1 |
| Jira | Turns action items into issues with a link back to the timestamp | Jira REST API, reusing the Incubarity Jira integration | v1 |
| Export | Markdown, Word, SRT/VTT captions, JSON | Local writers | v1 |
| Snapwright | Screenshots taken during a call appear on the meeting timeline | Watch Snapwright's capture folder, match by time | Later |
| switchhead | Link a meeting to a repo or PR; paste decisions into a PR description | Deep link plus shared meeting JSON | Later |
| Teams / Slack | Post the summary to a channel or chat | Graph / Slack API | Later |

## UI spec (for Claude Design)

switchscribe should look like a sibling of Switchyard and switchhead: a dense, dark-first developer tool with a three-pane layout, built for GPUI.

### Design principles

- **Same family:** reuse Switchyard's design tokens, typography, sidebar, tabs, tables, command palette and agent panel.
- **Transcript first:** the transcript is the main surface; everything else (summary, audio, people) hangs off it.
- **Speaker color is identity:** each known person keeps one color from an 8-color palette across all meetings. Unknown speakers use neutral gray until named.
- **Red means recording:** red appears only while audio is being captured; never use it for errors (use amber).
- **Every claim links to a moment:** summary lines, action items and agent answers carry a timestamp chip that plays the audio.
- **Keyboard-first:** every action reachable from the command palette.

### App shell

- **Window:** minimum 960 × 600; sidebar and right panel collapsible.
- **Title bar:** app name, global search field, Record button (shows a timer and turns red while recording), CLI selector, settings gear.
- **Left sidebar (260 px):** Meetings list grouped by Today / Yesterday / This week / Older; each row shows title, time, duration, attendee avatars and a status icon (recording, processing, ready, needs labels). Secondary nav below: Action items, People, Search.
- **Main pane:** the selected screen.
- **Right panel (360 px):** Ask panel (the agent), toggled with a button or shortcut.

### Mini recorder (floating, always on top)

- Pill-shaped window, about 320 × 56, draggable, snaps to screen edges.
- Contents: red pulsing dot, elapsed time, two level meters labeled "You" and "Teams", Bookmark button, quick-note button, Pause, Stop.
- Expands on hover to show the meeting title and a one-line live caption.
- Tray icon mirrors state: idle, armed (meeting detected), recording, processing.

### Meeting detected prompt

- Windows toast: "Teams meeting detected: \<calendar title>" with Record and Ignore; a checkbox "Always record meetings with this title".

### Live meeting view

- Left two-thirds: live captions streaming, grouped by speaker with color chips; provisional text in a lighter weight.
- Right third: private notes editor (Markdown) with automatic timestamp anchors; bookmarks listed under the notes.
- Header strip: meeting title, attendees, elapsed time, Stop.

### Meeting detail (after the call)

- **Header:** title (editable), date, duration, source app, attendee avatars with speaker colors, buttons for Export, Send to Jira, Re-run summary.
- **Processing state:** a stepper with Capture → Transcribe → Identify speakers → Summarize, each with progress and an estimated time; partial results appear as soon as each step finishes.
- **Tabs:** Summary, Transcript, Action items, Notes.
- **Summary tab:** sections for Summary, Decisions, Action items, Open questions; every bullet ends with a timestamp chip.
- **Transcript tab:** segment rows with speaker chip, timestamp and text; hovering a row shows Play, Copy, Edit, Relabel speaker. Search-in-transcript bar at the top; filter by speaker.
- **Audio player (docked bottom):** waveform split into colored lanes per speaker, play/pause, speed (1×–2×), skip silence toggle; the active segment highlights and auto-scrolls in the transcript.

### Name a speaker

- Inline card at the top of the transcript: "Who is Speaker 2?" with three short sample clips (play buttons), attendee suggestions from the calendar ranked by voice match with a confidence label, a free-text name field, and a "Remember this voice" checkbox (on by default).
- Suggested matches show as a dashed chip ("Alex?") that one click confirms.

### People (voiceprints)

- Table: name, color swatch, meetings heard in, samples stored, last heard, actions (play sample, re-enroll, merge with another person, delete voiceprint).
- Your own entry pinned at the top with a Re-record my voice button.

### Action items

- Cross-meeting table: item, owner (speaker chip), due date, source meeting + timestamp, status dropdown (Open, In Jira, Done), Send to Jira button per row and in bulk.

### Search

- Results grouped by meeting: snippet with highlighted terms, speaker chip, timestamp; Enter jumps to that moment with the audio cued.
- Filters: person, date range, meeting type.

### Ask panel (agent)

- Chat with a scope toggle (This meeting / All meetings) and the active CLI shown at the top.
- Answers render citations as timestamp chips; suggested prompts on an empty panel ("Summarize decisions", "What did I commit to this week?").
- Agent actions that change things (create Jira issues, edit summary) show an approve/deny card first.

### Settings

- Audio: Teams capture mode (Teams only / whole system), mic device, test recording with playback.
- Detection: auto-detect meetings, prompt or auto-record, auto-stop delay.
- Models: installed models with size and speed, download, GPU on/off, live captions on/off.
- Agent: CLI choice and path, summary templates editor.
- Integrations: Microsoft account, Jira site and project, Snapwright folder.
- Storage & privacy: data folder, keep audio for N days, delete all data, consent notice text.

### Onboarding (first run)

1. Welcome and what stays local.
2. Pick mic, run a test capture of Teams and mic with live meters.
3. Choose and download a model (recommended one preselected based on GPU).
4. Enroll your voice: read a short paragraph for about 20 seconds.
5. Connect calendar and Jira (skippable).
6. Recording-consent reminder with the notice text to paste into meetings.

### States to design

- Empty: no meetings yet, with a Record button and a "Start a test recording" link.
- Errors (amber): Teams not detected, mic disconnected mid-call (recording continues on the Teams track), model missing, CLI not found.
- Low confidence: segments with uncertain speaker show a small "?" on the chip.

### Keyboard shortcuts

| Action | Shortcut |
| --- | --- |
| Start / stop recording (global) | Ctrl+Shift+R |
| Bookmark moment (global) | Ctrl+Shift+B |
| Command palette | Ctrl+K |
| Search all meetings | Ctrl+Shift+F |
| Play / pause | Space |
| Previous / next segment | J / K |
| Toggle Ask panel | Ctrl+J |

## Data, privacy and consent

All audio, transcripts and voiceprints stay on the machine; the only thing that leaves is what you send to your chosen CLI's model for summaries.

- **Layout:** one folder per meeting under `%APPDATA%\switchscribe\meetings\` with the two Opus tracks, `transcript.json`, `summary.md` and `notes.md`; a SQLite database (FTS5) indexes everything.
- **Voiceprints:** stored encrypted with Windows DPAPI, deletable per person. They may count as biometric data under some state laws, so delete-on-request must be easy.
- **Retention:** keep audio for a configurable number of days, keep transcripts until deleted; a "Delete everything" button in Settings.
- **Model egress:** the Settings and onboarding screens say plainly that summaries send transcript text to the configured CLI's provider; a local-model CLI keeps it fully offline.
- **Consent:** some states require every participant's consent to record. Check TRAC's policy before using it at work; ship a short notice text to paste into the Teams chat and show the recording indicator at all times.

## Milestones

Build in seven steps, each ending in something you can use on a real Teams call; hand them to Claude Code one at a time.

1. **M0 Skeleton:** Cargo workspace with the crates above, GPUI shell reusing Switchyard components, SQLite store, settings file. Done when the app opens with an empty meetings list.
2. **M1 Capture:** Teams process loopback plus mic to two Opus files, device picker, mini recorder, tray icon. Done when a 30-minute Teams call records both tracks cleanly.
3. **M2 Transcription:** VAD, post-call Whisper per track, merged transcript, audio player synced to segments, model manager. Done when a recorded call shows a readable transcript with playback.
4. **M3 Speakers:** diarization on the Teams track, Name-a-speaker card, People screen, voiceprint matching. Done when a colleague named once is auto-named in the next meeting.
5. **M4 Agent:** MCP server, CLI runner, summary templates, Summary and Action items tabs, Ask panel, FTS search. Done when a meeting gets a summary with working timestamp links.
6. **M5 Detection and integrations:** Teams auto-detect prompt, Graph calendar, Jira export, Markdown/Word/SRT export. Done when a calendar meeting is detected, named and its action items land in Jira.
7. **M6 Live and polish:** live captions, onboarding, error states, signed installer and auto-update. Done when a fresh Windows machine goes from install to first recorded meeting without help.

After v1: Linux capture via PipeWire for Omarchy, then Zoom and browser meetings.

## Risks and open questions

The biggest unknowns are whether Teams' audio lands in a capturable process tree and whether TRAC allows the recording and calendar access.

| Risk | Impact | Mitigation |
| --- | --- | --- |
| New Teams renders audio in a child process | Process capture records silence | Include-tree mode; fall back to system loopback; test in M1 first |
| Echo bleed on laptop speakers | Remote voices attributed to you | Reference-track matching, headset recommendation |
| Work laptop has no usable GPU | Slow post-call transcription | Smaller default model on CPU, transcribe in background |
| Overlapping speech, similar voices | Some segments mislabeled | Confidence markers, one-click relabel |
| Graph calendar needs tenant admin consent | No auto-naming or attendee hints | Manual meeting title, ICS import fallback |
| Company policy on recording or unsigned apps | Can't use at work | Ask IT early; code-sign the installer |

Open questions:

- [ ] Does TRAC allow recording Teams meetings locally, and what notice is required?
- [ ] Will IT consent to the Graph calendar scope for a personal tool?
- [ ] Keep audio forever, or only transcripts after a set number of days?
- [ ] Which CLI is the default for summaries?
- [ ] Open source like Emulsion, or private?

## Sources

- [Microsoft: Application loopback audio capture sample](https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/)
- [miniaudio issue #484 on process loopback minimum versions](https://www.github.com/mackron/miniaudio/issues/484)
- [sherpa-onnx Rust crate docs](https://docs.rs/sherpa-onnx)
- [polyvoice: speaker diarization for Rust](https://docs.rs/crate/polyvoice/latest)
