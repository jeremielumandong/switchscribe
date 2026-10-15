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

## M1 Capture

Teams process loopback plus mic to two Opus files, device picker, mini recorder, tray
icon. Done when a 30-minute Teams call records both tracks cleanly.

## M2 Transcription

VAD, post-call Whisper per track, merged transcript, audio player synced to segments,
model manager.

## M3 Speakers

Diarization on the Teams track, Name-a-speaker card, People screen, voiceprint matching.

## M4 Agent

MCP server, CLI runner, summary templates, Summary and Action items tabs, Ask panel, FTS
search filters.

## M5 Detection and integrations

Teams auto-detect prompt, Graph calendar, Jira export, Markdown/Word/SRT export.

## M6 Live and polish

Live captions, onboarding, error states, signed installer and auto-update.

## Follow-ups

- Virtualize the sidebar and transcript lists (`uniform_list`) before real meeting
  counts grow; M0 renders them as plain scroll views.
- Command palette (Ctrl+K) from switchhead's `palette.rs`.
- Port switchhead's rotating log file and crash reports.
- Windows icon resource (switchhead's `build.rs`) once there is an icon.
