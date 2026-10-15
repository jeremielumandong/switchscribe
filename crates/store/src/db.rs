//! The SQLite index: meetings, people, transcript segments and an FTS5 table over the
//! segment text. Schema changes are numbered migrations tracked in `user_version`.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension as _, params};

use crate::model::{
    Meeting, MeetingStatus, NewMeeting, NewSegment, Person, PersonStats, SearchFilter, SearchHit,
    Segment, StoredVoiceprint,
};
use crate::secret;
use crate::{Error, Result, io_err};

/// Marks the start of a matched term in [`SearchHit::snippet`].
pub const MATCH_START: char = '\u{2}';
/// Marks the end of a matched term in [`SearchHit::snippet`].
pub const MATCH_END: char = '\u{3}';

/// Migrations, in order; `user_version` is how many have run.
const MIGRATIONS: &[&str] = &[
    // 1: meetings, people, segments, full-text search over segments.
    r#"
    CREATE TABLE people (
        id      INTEGER PRIMARY KEY,
        name    TEXT NOT NULL,
        color   INTEGER,
        is_me   INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE meetings (
        id            TEXT PRIMARY KEY,
        title         TEXT NOT NULL,
        started_at    INTEGER NOT NULL,
        duration_secs INTEGER NOT NULL DEFAULT 0,
        source_app    TEXT NOT NULL DEFAULT 'Teams',
        status        TEXT NOT NULL
    );
    CREATE INDEX meetings_started ON meetings (started_at DESC);
    CREATE TABLE meeting_people (
        meeting_id TEXT NOT NULL REFERENCES meetings (id) ON DELETE CASCADE,
        person_id  INTEGER NOT NULL REFERENCES people (id) ON DELETE CASCADE,
        position   INTEGER NOT NULL,
        PRIMARY KEY (meeting_id, person_id)
    );
    CREATE TABLE segments (
        id             INTEGER PRIMARY KEY,
        meeting_id     TEXT NOT NULL REFERENCES meetings (id) ON DELETE CASCADE,
        start_ms       INTEGER NOT NULL,
        end_ms         INTEGER NOT NULL,
        person_id      INTEGER REFERENCES people (id) ON DELETE SET NULL,
        speaker_label  TEXT NOT NULL,
        text           TEXT NOT NULL,
        low_confidence INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX segments_meeting ON segments (meeting_id, start_ms);
    CREATE VIRTUAL TABLE segments_fts USING fts5 (
        text, content = 'segments', content_rowid = 'id', tokenize = 'unicode61'
    );
    CREATE TRIGGER segments_ai AFTER INSERT ON segments BEGIN
        INSERT INTO segments_fts (rowid, text) VALUES (new.id, new.text);
    END;
    CREATE TRIGGER segments_ad AFTER DELETE ON segments BEGIN
        INSERT INTO segments_fts (segments_fts, rowid, text) VALUES ('delete', old.id, old.text);
    END;
    CREATE TRIGGER segments_au AFTER UPDATE OF text ON segments BEGIN
        INSERT INTO segments_fts (segments_fts, rowid, text) VALUES ('delete', old.id, old.text);
        INSERT INTO segments_fts (rowid, text) VALUES (new.id, new.text);
    END;
    "#,
    // 2: voiceprints (sealed with DPAPI on Windows), one per person.
    r#"
    CREATE TABLE voiceprints (
        person_id       INTEGER PRIMARY KEY REFERENCES people (id) ON DELETE CASCADE,
        embedding       BLOB NOT NULL,
        samples         INTEGER NOT NULL,
        sample_meeting  TEXT REFERENCES meetings (id) ON DELETE SET NULL,
        sample_start_ms INTEGER,
        sample_end_ms   INTEGER,
        updated_at      INTEGER NOT NULL
    );
    CREATE INDEX segments_person ON segments (person_id);
    "#,
    // 3: summaries (written by the coding CLI), action items, and changes the Ask
    // panel's agent proposed that wait for the user's approval.
    r#"
    CREATE TABLE summaries (
        meeting_id     TEXT PRIMARY KEY REFERENCES meetings (id) ON DELETE CASCADE,
        template       TEXT NOT NULL,
        cli            TEXT NOT NULL,
        generated_at   INTEGER NOT NULL,
        body           TEXT NOT NULL
    );
    CREATE TABLE action_items (
        id          INTEGER PRIMARY KEY,
        meeting_id  TEXT NOT NULL REFERENCES meetings (id) ON DELETE CASCADE,
        position    INTEGER NOT NULL,
        text        TEXT NOT NULL,
        owner_id    INTEGER REFERENCES people (id) ON DELETE SET NULL,
        owner_name  TEXT,
        due         TEXT,
        at_ms       INTEGER NOT NULL,
        status      TEXT NOT NULL DEFAULT 'open'
    );
    CREATE INDEX action_items_meeting ON action_items (meeting_id, position);
    CREATE TABLE pending_actions (
        id          INTEGER PRIMARY KEY,
        tool        TEXT NOT NULL,
        meeting_id  TEXT REFERENCES meetings (id) ON DELETE CASCADE,
        payload     TEXT NOT NULL,
        created_at  INTEGER NOT NULL
    );
    "#,
];

/// An open index.
pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    /// Open (or create) the index at `path` and bring its schema up to date.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io_err(dir))?;
        }
        let conn = Connection::open(path)?;
        // WAL lets the MCP server read while the app writes.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    /// An empty index in memory (tests, the demo data).
    pub fn open_in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.pragma_update(None, "foreign_keys", true)?;
        // The MCP server and the app share the file; wait for each other's writes.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        let supported = MIGRATIONS.len() as i64;
        if version > supported {
            return Err(Error::NewerSchema {
                found: version,
                supported,
            });
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", i as i64 + 1)?;
            tx.commit()?;
            tracing::info!(version = i + 1, "store schema migrated");
        }
        Ok(())
    }

    /// The schema version (number of migrations applied).
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    /// Add a person; returns their id.
    pub fn add_person(&self, name: &str, color: Option<u8>, is_me: bool) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO people (name, color, is_me) VALUES (?1, ?2, ?3)",
            params![name, color, is_me],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Everyone, the user first, then by name.
    pub fn people(&self) -> Result<Vec<Person>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, color, is_me FROM people ORDER BY is_me DESC, name")?;
        let rows = stmt.query_map([], person_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Rename a person.
    pub fn rename_person(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE people SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    /// Remove a person: their segments keep their speaker label but lose the name, and
    /// their voiceprint goes with them.
    pub fn delete_person(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM people WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Fold `from` into `into`: segments and meeting attendance move over, then `from`
    /// is deleted. The caller merges the voiceprints.
    pub fn merge_people(&mut self, from: i64, into: i64) -> Result<()> {
        if from == into {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE segments SET person_id = ?2 WHERE person_id = ?1",
            params![from, into],
        )?;
        tx.execute(
            "UPDATE OR IGNORE meeting_people SET person_id = ?2 WHERE person_id = ?1",
            params![from, into],
        )?;
        tx.execute("DELETE FROM people WHERE id = ?1", [from])?;
        tx.commit()?;
        Ok(())
    }

    /// Add a person to a meeting's attendees (after the last), if not already there.
    pub fn add_attendee(&self, meeting_id: &str, person_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO meeting_people (meeting_id, person_id, position)
             VALUES (?1, ?2, (SELECT COALESCE(MAX(position) + 1, 0)
                              FROM meeting_people WHERE meeting_id = ?1))",
            params![meeting_id, person_id],
        )?;
        Ok(())
    }

    /// Replace a meeting's transcript (after speakers are identified).
    pub fn replace_segments(&mut self, meeting_id: &str, segments: &[NewSegment]) -> Result<()> {
        self.conn
            .execute("DELETE FROM segments WHERE meeting_id = ?1", [meeting_id])?;
        self.add_segments(meeting_id, segments)
    }

    /// Name every segment of a meeting with speaker label `label`; `None` unnames them.
    /// Returns how many segments changed.
    pub fn assign_speaker(
        &self,
        meeting_id: &str,
        label: &str,
        person_id: Option<i64>,
    ) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE segments SET person_id = ?3, low_confidence = 0
             WHERE meeting_id = ?1 AND speaker_label = ?2",
            params![meeting_id, label, person_id],
        )?)
    }

    /// Every voiceprint this user can read.
    pub fn voiceprints(&self) -> Result<Vec<StoredVoiceprint>> {
        let mut stmt = self.conn.prepare(
            "SELECT person_id, embedding, samples, sample_meeting, sample_start_ms, sample_end_ms
             FROM voiceprints",
        )?;
        let rows = stmt.query_map([], |r| {
            let blob: Vec<u8> = r.get(1)?;
            let meeting: Option<String> = r.get(3)?;
            let (start, end): (Option<i64>, Option<i64>) = (r.get(4)?, r.get(5)?);
            Ok((
                r.get::<_, i64>(0)?,
                blob,
                r.get::<_, u32>(2)?,
                meeting.zip(start).zip(end).map(|((m, s), e)| (m, s, e)),
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (person_id, blob, samples, sample) = row?;
            if let Some(bytes) = secret::open(&blob) {
                out.push(StoredVoiceprint {
                    person_id,
                    embedding: secret::bytes_to_floats(&bytes),
                    samples,
                    sample,
                });
            }
        }
        Ok(out)
    }

    /// Store a person's voiceprint, sealed, replacing any previous one. A `sample` of
    /// `None` keeps the stored sample moment.
    pub fn set_voiceprint(&self, v: &StoredVoiceprint) -> Result<()> {
        let blob = secret::seal(&secret::floats_to_bytes(&v.embedding));
        let (m, s, e) = match &v.sample {
            Some((m, s, e)) => (Some(m.as_str()), Some(*s), Some(*e)),
            None => (None, None, None),
        };
        self.conn.execute(
            "INSERT INTO voiceprints
             (person_id, embedding, samples, sample_meeting, sample_start_ms, sample_end_ms, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s', 'now'))
             ON CONFLICT (person_id) DO UPDATE SET
                 embedding = excluded.embedding,
                 samples = excluded.samples,
                 sample_meeting = COALESCE(excluded.sample_meeting, sample_meeting),
                 sample_start_ms = COALESCE(excluded.sample_start_ms, sample_start_ms),
                 sample_end_ms = COALESCE(excluded.sample_end_ms, sample_end_ms),
                 updated_at = excluded.updated_at",
            params![v.person_id, blob, v.samples, m, s, e],
        )?;
        Ok(())
    }

    /// Delete a person's voiceprint (they stay, with their name, in past meetings).
    pub fn delete_voiceprint(&self, person_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM voiceprints WHERE person_id = ?1", [person_id])?;
        Ok(())
    }

    /// Per person: meetings heard in, when last heard, and their voiceprint's size.
    pub fn people_stats(&self) -> Result<Vec<PersonStats>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id,
                    (SELECT COUNT(DISTINCT s.meeting_id) FROM segments s WHERE s.person_id = p.id),
                    (SELECT MAX(m.started_at) FROM segments s JOIN meetings m ON m.id = s.meeting_id
                     WHERE s.person_id = p.id),
                    COALESCE(v.samples, 0), v.sample_meeting, v.sample_start_ms, v.sample_end_ms
             FROM people p LEFT JOIN voiceprints v ON v.person_id = p.id",
        )?;
        let rows = stmt.query_map([], |r| {
            let meeting: Option<String> = r.get(4)?;
            let (start, end): (Option<i64>, Option<i64>) = (r.get(5)?, r.get(6)?);
            Ok(PersonStats {
                person_id: r.get(0)?,
                meetings: r.get(1)?,
                last_heard: r.get(2)?,
                samples: r.get(3)?,
                sample: meeting.zip(start).zip(end).map(|((m, s), e)| (m, s, e)),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Add a meeting and its attendees.
    pub fn insert_meeting(&mut self, m: &NewMeeting) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO meetings (id, title, started_at, duration_secs, source_app, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                m.id,
                m.title,
                m.started_at,
                m.duration_secs,
                m.source_app,
                m.status.as_str()
            ],
        )?;
        for (pos, person) in m.attendees.iter().enumerate() {
            tx.execute(
                "INSERT INTO meeting_people (meeting_id, person_id, position) VALUES (?1, ?2, ?3)",
                params![m.id, person, pos as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Set a meeting's status, for example `Processing` once its recording stops.
    pub fn set_status(&self, id: &str, status: MeetingStatus) -> Result<()> {
        self.conn.execute(
            "UPDATE meetings SET status = ?2 WHERE id = ?1",
            params![id, status.as_str()],
        )?;
        Ok(())
    }

    /// Set a meeting's recorded length.
    pub fn set_duration(&self, id: &str, duration_secs: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE meetings SET duration_secs = ?2 WHERE id = ?1",
            params![id, duration_secs],
        )?;
        Ok(())
    }

    /// Rename a meeting.
    pub fn rename_meeting(&self, id: &str, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE meetings SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        Ok(())
    }

    /// Every meeting, newest first, with its attendees.
    pub fn meetings(&self) -> Result<Vec<Meeting>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, started_at, duration_secs, source_app, status
             FROM meetings ORDER BY started_at DESC, id",
        )?;
        let mut meetings: Vec<Meeting> = stmt
            .query_map([], meeting_row)?
            .collect::<rusqlite::Result<_>>()?;
        let mut att = self.conn.prepare(
            "SELECT mp.meeting_id, p.id, p.name, p.color, p.is_me
             FROM meeting_people mp JOIN people p ON p.id = mp.person_id
             ORDER BY mp.meeting_id, mp.position",
        )?;
        let mut rows = att.query([])?;
        while let Some(r) = rows.next()? {
            let meeting_id: String = r.get(0)?;
            let person = Person {
                id: r.get(1)?,
                name: r.get(2)?,
                color: r.get(3)?,
                is_me: r.get(4)?,
            };
            if let Some(m) = meetings.iter_mut().find(|m| m.id == meeting_id) {
                m.attendees.push(person);
            }
        }
        Ok(meetings)
    }

    /// One meeting by id.
    pub fn meeting(&self, id: &str) -> Result<Option<Meeting>> {
        let m = self
            .conn
            .query_row(
                "SELECT id, title, started_at, duration_secs, source_app, status
                 FROM meetings WHERE id = ?1",
                [id],
                meeting_row,
            )
            .optional()?;
        let Some(mut m) = m else { return Ok(None) };
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, p.color, p.is_me
             FROM meeting_people mp JOIN people p ON p.id = mp.person_id
             WHERE mp.meeting_id = ?1 ORDER BY mp.position",
        )?;
        m.attendees = stmt
            .query_map([id], person_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Some(m))
    }

    /// Append transcript segments to a meeting.
    pub fn add_segments(&mut self, meeting_id: &str, segments: &[NewSegment]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO segments
                 (meeting_id, start_ms, end_ms, person_id, speaker_label, text, low_confidence)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for s in segments {
                stmt.execute(params![
                    meeting_id,
                    s.start_ms,
                    s.end_ms,
                    s.person_id,
                    s.speaker_label,
                    s.text,
                    s.low_confidence
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A meeting's transcript, in time order.
    pub fn segments(&self, meeting_id: &str) -> Result<Vec<Segment>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, start_ms, end_ms, person_id, speaker_label, text, low_confidence
             FROM segments WHERE meeting_id = ?1 ORDER BY start_ms, id",
        )?;
        let rows = stmt.query_map([meeting_id], |r| {
            Ok(Segment {
                id: r.get(0)?,
                start_ms: r.get(1)?,
                end_ms: r.get(2)?,
                person_id: r.get(3)?,
                speaker_label: r.get(4)?,
                text: r.get(5)?,
                low_confidence: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Full-text search over every transcript, best matches first. The query is taken as
    /// plain words (each one a prefix match), never as FTS5 syntax, so any input is safe.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        self.search_with(query, &SearchFilter::default(), limit)
    }

    /// [`Store::search`], narrowed to one person and/or meetings since a time.
    pub fn search_with(
        &self,
        query: &str,
        filter: &SearchFilter,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let Some(fts) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT s.meeting_id, m.title, s.start_ms, s.person_id, s.speaker_label,
                    snippet(segments_fts, 0, char(2), char(3), '…', 16)
             FROM segments_fts
             JOIN segments s ON s.id = segments_fts.rowid
             JOIN meetings m ON m.id = s.meeting_id
             WHERE segments_fts MATCH ?1
               AND (?3 IS NULL OR s.person_id = ?3)
               AND (?4 IS NULL OR m.started_at >= ?4)
               AND (?5 IS NULL OR m.id = ?5)
             ORDER BY bm25(segments_fts), m.started_at DESC, s.start_ms
             LIMIT ?2",
        )?;
        let args = params![
            fts,
            limit as i64,
            filter.person,
            filter.since,
            filter.meeting.as_deref()
        ];
        let rows = stmt.query_map(args, |r| {
            Ok(SearchHit {
                meeting_id: r.get(0)?,
                meeting_title: r.get(1)?,
                start_ms: r.get(2)?,
                person_id: r.get(3)?,
                speaker_label: r.get(4)?,
                snippet: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// Words of `query` as quoted FTS5 prefix terms (`"cut"* "over"*`); `None` when there
/// are no words.
fn fts_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

fn person_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Person> {
    Ok(Person {
        id: r.get(0)?,
        name: r.get(1)?,
        color: r.get(2)?,
        is_me: r.get(3)?,
    })
}

fn meeting_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Meeting> {
    let status: String = r.get(5)?;
    Ok(Meeting {
        id: r.get(0)?,
        title: r.get(1)?,
        started_at: r.get(2)?,
        duration_secs: r.get(3)?,
        source_app: r.get(4)?,
        status: MeetingStatus::parse(&status),
        attendees: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start_s: i64, person: Option<i64>, text: &str) -> NewSegment {
        NewSegment {
            start_ms: start_s * 1000,
            end_ms: start_s * 1000 + 4000,
            person_id: person,
            speaker_label: if person.is_some() {
                String::new()
            } else {
                "Speaker 3".into()
            },
            text: text.into(),
            low_confidence: false,
        }
    }

    fn meeting(id: &str, started_at: i64, attendees: Vec<i64>) -> NewMeeting {
        NewMeeting {
            id: id.into(),
            title: format!("Meeting {id}"),
            started_at,
            duration_secs: 600,
            source_app: "Teams".into(),
            status: MeetingStatus::Ready,
            attendees,
        }
    }

    #[test]
    fn new_store_is_empty_and_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data").join("switchscribe.db");
        let store = Store::open(&path).unwrap();
        assert!(store.meetings().unwrap().is_empty());
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
        drop(store);
        // Reopening runs no migration twice.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
    }

    #[test]
    fn refuses_a_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchscribe.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 99).unwrap();
        }
        assert!(matches!(
            Store::open(&path),
            Err(Error::NewerSchema { found: 99, .. })
        ));
    }

    #[test]
    fn meetings_newest_first_with_attendees_in_order() {
        let mut store = Store::open_in_memory().unwrap();
        let me = store.add_person("Jeremie", Some(0), true).unwrap();
        let priya = store.add_person("Priya Nair", Some(1), false).unwrap();
        store
            .insert_meeting(&meeting("old", 100, vec![me]))
            .unwrap();
        store
            .insert_meeting(&meeting("new", 200, vec![priya, me]))
            .unwrap();
        let all = store.meetings().unwrap();
        assert_eq!(
            all.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["new", "old"]
        );
        let names: Vec<&str> = all[0].attendees.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Priya Nair", "Jeremie"]);
        let one = store.meeting("new").unwrap().unwrap();
        assert_eq!(one, all[0]);
        assert!(store.meeting("missing").unwrap().is_none());
        assert_eq!(store.people().unwrap()[0].name, "Jeremie");
    }

    #[test]
    fn segments_and_search() {
        let mut store = Store::open_in_memory().unwrap();
        let me = store.add_person("Jeremie", Some(0), true).unwrap();
        store
            .insert_meeting(&meeting("aks", 100, vec![me]))
            .unwrap();
        store
            .add_segments(
                "aks",
                &[
                    seg(236, Some(me), "We cut over the two pools on the 22nd."),
                    seg(
                        14,
                        Some(me),
                        "Main thing today is the AKS migration cutover.",
                    ),
                    seg(118, None, "Can we cut over without the GPU pool?"),
                ],
            )
            .unwrap();
        let segs = store.segments("aks").unwrap();
        assert_eq!(
            segs.iter().map(|s| s.start_ms).collect::<Vec<_>>(),
            [14_000, 118_000, 236_000]
        );
        assert_eq!(segs[1].speaker_label, "Speaker 3");

        let hits = store.search("cutover", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].start_ms, 14_000);
        assert!(hits[0].snippet.contains("\u{2}cutover\u{3}"));
        // Prefix match, several words, punctuation and FTS syntax are all plain words.
        assert_eq!(store.search("gpu po", 10).unwrap().len(), 1);
        assert_eq!(store.search("cut\" (", 10).unwrap().len(), 3);
        assert!(store.search("  ", 10).unwrap().is_empty());
    }

    #[test]
    fn status_duration_and_title_update() {
        let mut s = Store::open_in_memory().unwrap();
        let mut m = meeting("rec", 100, vec![]);
        m.status = MeetingStatus::Recording;
        s.insert_meeting(&m).unwrap();
        s.set_status("rec", MeetingStatus::Processing).unwrap();
        s.set_duration("rec", 1834).unwrap();
        s.rename_meeting("rec", "Cerberus rollout sync").unwrap();
        let got = s.meeting("rec").unwrap().unwrap();
        assert_eq!(got.status, MeetingStatus::Processing);
        assert_eq!(got.duration_secs, 1834);
        assert_eq!(got.title, "Cerberus rollout sync");
    }

    #[test]
    fn deleting_a_meeting_removes_its_segments_from_search() {
        let mut store = Store::open_in_memory().unwrap();
        store.insert_meeting(&meeting("m", 1, vec![])).unwrap();
        store
            .add_segments("m", &[seg(1, None, "Datadog dashboards")])
            .unwrap();
        assert_eq!(store.search("datadog", 5).unwrap().len(), 1);
        store
            .conn
            .execute("DELETE FROM meetings WHERE id = 'm'", [])
            .unwrap();
        assert!(store.search("datadog", 5).unwrap().is_empty());
    }

    #[test]
    fn voiceprints_people_and_speakers() {
        let mut store = Store::open_in_memory().unwrap();
        let me = store.add_person("Jeremie", Some(0), true).unwrap();
        let priya = store.add_person("Priya", Some(1), false).unwrap();
        store.insert_meeting(&meeting("m1", 100, vec![me])).unwrap();
        store
            .add_segments(
                "m1",
                &[
                    seg(0, Some(me), "hello"),
                    NewSegment {
                        speaker_label: "Speaker 1".into(),
                        low_confidence: true,
                        ..seg(5, None, "hi there")
                    },
                ],
            )
            .unwrap();
        assert_eq!(
            store
                .assign_speaker("m1", "Speaker 1", Some(priya))
                .unwrap(),
            1
        );
        store.add_attendee("m1", priya).unwrap();
        store.add_attendee("m1", priya).unwrap();
        let m = store.meeting("m1").unwrap().unwrap();
        assert_eq!(
            m.attendees.iter().map(|p| p.id).collect::<Vec<_>>(),
            [me, priya]
        );
        let segs = store.segments("m1").unwrap();
        assert_eq!(segs[1].person_id, Some(priya));
        assert!(!segs[1].low_confidence);

        let v = StoredVoiceprint {
            person_id: priya,
            embedding: vec![0.5, -0.25],
            samples: 1,
            sample: Some(("m1".into(), 5000, 6000)),
        };
        store.set_voiceprint(&v).unwrap();
        // A later update without a sample keeps the stored one.
        store
            .set_voiceprint(&StoredVoiceprint {
                samples: 2,
                sample: None,
                ..v.clone()
            })
            .unwrap();
        let got = store.voiceprints().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].embedding, v.embedding);
        assert_eq!(got[0].samples, 2);
        assert_eq!(got[0].sample, v.sample);

        let stats = store.people_stats().unwrap();
        let ps = stats.iter().find(|s| s.person_id == priya).unwrap();
        assert_eq!((ps.meetings, ps.last_heard, ps.samples), (1, Some(100), 2));

        // Merging moves segments and attendance; deleting a voiceprint keeps the person.
        let dup = store.add_person("Priya N", Some(2), false).unwrap();
        store.assign_speaker("m1", "Speaker 1", Some(dup)).unwrap();
        store.add_attendee("m1", dup).unwrap();
        store.merge_people(dup, priya).unwrap();
        assert_eq!(store.segments("m1").unwrap()[1].person_id, Some(priya));
        assert_eq!(store.people().unwrap().len(), 2);
        store.delete_voiceprint(priya).unwrap();
        assert!(store.voiceprints().unwrap().is_empty());
        store.rename_person(priya, "Priya Nair").unwrap();
        store.delete_person(priya).unwrap();
        assert_eq!(store.segments("m1").unwrap()[1].person_id, None);

        // Replacing segments re-indexes search.
        store
            .replace_segments("m1", &[seg(1, None, "replacement words")])
            .unwrap();
        assert_eq!(store.search("hello", 10).unwrap().len(), 0);
        assert_eq!(store.search("replacement", 10).unwrap().len(), 1);
    }
}
