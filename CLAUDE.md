# CLAUDE.md — switchscribe

switchscribe is a local-first meeting recorder and transcriber in Rust/GPUI: it captures
Teams calls on Windows without a bot, transcribes on-device, names speakers, and turns
meetings into searchable notes and action items a coding CLI can query over MCP. It is a
sibling of switchhead (github.com/jeremielumandong/switchhead) and Switchyard and shares
their GPUI stack and design system.

- Build plan: `docs/BUILD-PLAN.md` · milestones and follow-ups: `PLAN.md`
- Decisions log: `docs/DECISIONS.md` (append when you make a non-obvious call)
- UI prototype: `docs/design/switchscribe.dc.html` (the source for sizes and colors)

## How to work in this repo

1. Take the next milestone in `PLAN.md`; build milestones one at a time.
2. Implement only that milestone. Note drive-by ideas under "Follow-ups" in `PLAN.md`.
3. Run the checks below. All must pass.
4. Tick the milestone's items in `PLAN.md`; record non-obvious calls in `docs/DECISIONS.md`.

## Commands

```bash
cargo build --workspace
cargo run -p switchscribe-app                # binary: `switchscribe`
cargo run -p switchscribe-app -- --demo      # prototype data, in memory
SWITCHSCRIBE_FAKE_AUDIO=1 cargo run -p switchscribe-app   # record synthetic voices
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Workspace layout

Dependency direction: `app → everything`; `pipeline, agent, integrations → store`.
Nothing depends on `app`.

| Crate | Job |
| --- | --- |
| `app` | GPUI shell, screens, mini recorder, tray icon |
| `capture` | Teams process loopback, mic, meeting detection |
| `pipeline` | VAD, transcription, diarization, track merge (`SpeechEngine`, `Diarizer` traits) |
| `speakers` | Voiceprint library and matching |
| `store` | Data folder, SQLite + FTS5 index, settings file |
| `agent` | MCP server, CLI runner, summary templates |
| `integrations` | Graph calendar, Jira, exports |

## Rules

- **Local first.** Audio, transcripts and voiceprints never leave the machine; only text
  sent to the user's chosen CLI does.
- **The UI thread never does I/O.** Store reads and settings writes run on GPUI's
  background executor; startup reads happen before the window opens.
- **Red means recording.** Never use red for errors; warnings use amber.
- **Every claim links to a moment.** Summary lines, action items and answers carry a
  timestamp chip.
- **One design system.** Port Switchyard / switchhead UI modules rather than inventing
  new ones. Fixed sizes use `appearance::rpx`, text sizes `appearance::ts::*`.
- **Schema changes are new migrations** appended to `MIGRATIONS` in
  `crates/store/src/db.rs`; never edit a shipped one.

## Commits

- Author every commit as the repository owner: `Jeremie Lumandong <jeremie.lumandong@outlook.com>`.
  Before the first commit in a fresh clone run
  `git config user.name "Jeremie Lumandong"` and
  `git config user.email "jeremie.lumandong@outlook.com"` (the container's default identity
  is Claude's). Jeremie is the only author: no `Co-Authored-By: Claude ...` or
  `Claude-Session:` trailers in commit messages, and no "Generated with Claude Code" footer
  or session link in PR descriptions (the attribution header comment at the top of a PR body
  stays).

## Coding conventions

- Edition 2024, toolchain 1.98.1 (`rust-version` 1.97).
- Errors: `thiserror` enums per library crate; `anyhow` only in the app's top level.
- No `unwrap()` / `expect()` outside tests.
- Logging with `tracing`. Public items get doc comments. Unit tests next to the code.

## Gotchas

- Linking the app on Linux needs `libxkbcommon-dev` and `libxkbcommon-x11-dev`. GPUI
  renders through Vulkan: under Xvfb install `mesa-vulkan-drivers` (lavapipe).
- libopus is built from source (`opusic-sys`), so builds need CMake. Windows-only code
  can be checked from Linux with `rustup target add x86_64-pc-windows-gnu`, the
  `gcc-mingw-w64-x86-64` package and `cargo clippy --target x86_64-pc-windows-gnu`.
- Capture threads and device listing join COM's MTA on their own threads; never call
  WASAPI on the UI thread.
- Screenshots: `Xvfb :99 -screen 0 1440x900x24 &`, then
  `DISPLAY=:99 SWITCHSCRIBE_DATA=<tmp> switchscribe --demo` and
  `import -window root shot.png` (ImageMagick).
- GPUI Kit 0.7.3 runs on the patched GPUI Fast stack in `vendor/`; do not enable
  gpui-kit's own `gpui-fast` feature, and update `vendor/` from switchhead.
- A `uniform_list` row sizes to its content, so rows with a `flex_1` column need
  `.w_full()`. Text in a flex row needs `min_w_0()` to truncate.
- Title bar controls need `.occlude()`, or Windows takes clicks on them as window drags.
