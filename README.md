# switchscribe

**A local-first meeting recorder and transcriber for Teams, in Rust and GPUI.**

switchscribe records Teams calls on Windows without a bot joining, transcribes them on
your machine, names who spoke, and turns each meeting into searchable notes and action
items that your coding CLI (Claude Code, Codex, Gemini CLI) can query. It is a sibling of
[switchhead](https://github.com/jeremielumandong/switchhead) and Switchyard and shares
their GPUI stack and design system.

> Early development. **M0 (skeleton)** and **M1 (capture)** are in: the app shell, the
> SQLite store, and recording Teams plus your mic to two Opus tracks with a live view,
> a floating mini recorder and a tray icon. Transcription arrives in M2. See
> [`PLAN.md`](PLAN.md) and the full [build plan](docs/BUILD-PLAN.md).

![The demo data: a meeting's summary with the Ask panel open](docs/screenshots/demo-summary.png)

![Recording: the live view, notes and the mini recorder](docs/screenshots/recording-live.png)

## Run it

```bash
cargo run -p switchscribe-app             # binary: `switchscribe`
cargo run -p switchscribe-app -- --demo   # the prototype's meetings, in memory only
```

Building needs CMake (libopus is compiled from source). Linux needs `libxkbcommon-dev`, `libxkbcommon-x11-dev`, `libwayland-dev`, `libxcb1-dev`,
`libvulkan-dev` and `libfontconfig-dev` to build.

| Variable | Effect |
| --- | --- |
| `SWITCHSCRIBE_DATA` | Data folder (default `%APPDATA%\switchscribe`, or the platform data dir) |
| `SWITCHSCRIBE_DEMO=1` | Same as `--demo` |
| `SWITCHSCRIBE_FAKE_AUDIO=1` | Record two synthetic voices instead of the devices (any OS; the demo always does) |
| `SWITCHSCRIBE_ZOOM` | UI zoom, `0.7`–`2.0` |
| `SWITCHSCRIBE_LOG` | Log filter, e.g. `debug` |

## Layout

```
crates/
  app/           switchscribe-app           GPUI binary `switchscribe`: shell, screens
  capture/       switchscribe-capture       Teams loopback + mic to Opus tracks (WASAPI)
  pipeline/      switchscribe-pipeline      VAD, transcription, diarization, merge (M2, M3)
  speakers/      switchscribe-speakers      voiceprints and matching (M3)
  store/         switchscribe-store         data folder, SQLite + FTS5, settings
  agent/         switchscribe-agent         MCP server, CLI runner, templates (M4)
  integrations/  switchscribe-integrations  Outlook calendar, Jira, exports (M5)
vendor/          the application-patched GPUI Fast stack (shared with switchhead)
docs/            build plan, decisions, the UI prototype
```

## Your data

Everything stays on your machine: one folder per meeting under the data folder's
`meetings/` (`mic.opus`, `remote.opus`, `capture.json`, `notes.md`), a SQLite index
(`switchscribe.db`) and `settings.json`. The only thing that
ever leaves is transcript text you send to your chosen CLI's model for summaries.

## License

MIT. GPUI and gpui-component are Apache-2.0; Geist is OFL
(`crates/app/assets/fonts/OFL-Geist.txt`).
