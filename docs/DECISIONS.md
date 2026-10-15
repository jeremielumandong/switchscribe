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
