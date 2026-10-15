//! Speakers: identifying them after transcription, the Name-a-speaker card at the top of
//! a transcript, and the People screen (the voiceprint library).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement as _, ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window,
    div, px,
};
use switchscribe_pipeline::TranscriptFile;
use switchscribe_pipeline::identify::{self, Cluster, Known, SpeakersFile};
use switchscribe_pipeline::models::{EMBEDDING_MODEL, SEGMENTATION_MODEL};
use switchscribe_pipeline::sherpa::{SherpaDiarizer, SherpaEmbedder};
use switchscribe_speakers::{Embedding, Thresholds, Voiceprint, next_color, rank};
use switchscribe_store::{
    Meeting, MeetingStatus, Person, PersonStats, Segment, Store, StoredVoiceprint,
};

use crate::appearance::{rpx, ts};
use crate::demo;
use crate::format;
use crate::recording::Banner;
use crate::theme::Palette;
use crate::ui::{self, Kind};
use crate::workspace::{Screen, SharedStore, Workspace};

/// Speaker state the workspace keeps.
pub struct Speakers {
    /// The selected meeting's `speakers.json`.
    pub file: Option<(String, SpeakersFile)>,
    /// The voiceprint library.
    pub voiceprints: Vec<StoredVoiceprint>,
    /// Per-person numbers for the People screen.
    pub stats: Vec<PersonStats>,
    /// A speaker the user chose to relabel: meeting id and label.
    relabel: Option<(String, String)>,
    /// Speakers put off with "Later" this session.
    later: HashSet<(String, String)>,
    /// "Remember this voice".
    remember: bool,
    /// The card's "Someone else…" field.
    name_input: Entity<InputState>,
    /// People screen: the person being merged into someone else.
    merging: Option<i64>,
    /// A naming or library change is being written.
    busy: bool,
    /// Pause playback here (a clip is playing), milliseconds.
    pub clip_end: Option<u64>,
    /// A clip to play once the meeting's audio has loaded.
    pending_clip: Option<(String, i64, i64)>,
    /// Calendar invitees of the selected meeting who are not in the library yet.
    pub invitees: Vec<String>,
}

impl Speakers {
    /// Empty, with the library read at startup.
    pub fn new(
        voiceprints: Vec<StoredVoiceprint>,
        stats: Vec<PersonStats>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        Self {
            file: None,
            voiceprints,
            stats,
            relabel: None,
            later: HashSet::new(),
            remember: true,
            name_input: cx.new(|cx| InputState::new(window, cx).placeholder("Someone else…")),
            merging: None,
            busy: false,
            clip_end: None,
            pending_clip: None,
            invitees: Vec::new(),
        }
    }
}

/// The speaker models are in `models_dir`.
pub fn models_installed(installed: &HashSet<&'static str>) -> bool {
    installed.contains(SEGMENTATION_MODEL.id) && installed.contains(EMBEDDING_MODEL.id)
}

/// Make sure the user exists as a person (the mic track is theirs), named after the
/// account they are signed in as.
pub fn ensure_me(store: &Store) -> switchscribe_store::Result<()> {
    let people = store.people()?;
    if people.iter().any(|p| p.is_me) {
        return Ok(());
    }
    let name = ["USERNAME", "USER"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "Me".to_owned());
    let color = next_color(people.iter().map(|p| p.color));
    store.add_person(name.trim(), Some(color), true)?;
    Ok(())
}

/// Identify the speakers of a transcribed meeting: diarize the Teams track, match the
/// clusters against the library, store the labelled transcript and `speakers.json`, and
/// keep the user's voiceprint current. Returns the meeting's new status. Runs on a worker
/// thread.
pub fn identify_meeting(
    dir: &Path,
    models_dir: &Path,
    store: &SharedStore,
    meeting_id: &str,
    progress: &dyn Fn(f32),
    cancel: &AtomicBool,
) -> Result<MeetingStatus, String> {
    let locked = || {
        store
            .lock()
            .map_err(|_| "the index is unavailable".to_owned())
    };
    let transcript = TranscriptFile::load(dir).map_err(|e| e.to_string())?;
    let (library, candidates, me) = {
        let s = locked()?;
        let me = s
            .people()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|p| p.is_me)
            .map(|p| p.id);
        let attendees: Vec<i64> = s
            .meeting(meeting_id)
            .map_err(|e| e.to_string())?
            .map(|m| m.attendees.iter().map(|a| a.id).collect())
            .unwrap_or_default();
        let library = s.voiceprints().map_err(|e| e.to_string())?;
        (library, attendees, me)
    };
    // The user is on the mic track; a Teams voice is someone else.
    let others: Vec<Voiceprint> = library
        .iter()
        .filter(|v| Some(v.person_id) != me)
        .map(to_voiceprint)
        .collect();
    let diarizer = SherpaDiarizer::load(
        &SEGMENTATION_MODEL.path(models_dir),
        &EMBEDDING_MODEL.path(models_dir),
    )
    .map_err(|e| e.to_string())?;
    let embedder =
        SherpaEmbedder::load(&EMBEDDING_MODEL.path(models_dir)).map_err(|e| e.to_string())?;
    let known = Known {
        library: &others,
        candidates: &candidates,
        me,
        thresholds: Thresholds::default(),
    };
    let found = identify::identify_speakers(
        dir,
        &transcript,
        &diarizer,
        &embedder,
        &known,
        &mut |f| progress(f),
        cancel,
    )
    .map_err(|e| e.to_string())?;
    found.file.save(dir).map_err(|e| e.to_string())?;

    let status = if found.file.unnamed().next().is_some() {
        MeetingStatus::NeedsLabels
    } else {
        MeetingStatus::Ready
    };
    let mut s = locked()?;
    s.replace_segments(meeting_id, &found.segments)
        .map_err(|e| e.to_string())?;
    if let (Some(me), Some(sample)) = (me, &found.file.me) {
        let mut v = library
            .iter()
            .find(|v| v.person_id == me)
            .map_or_else(|| empty_voiceprint(me), to_voiceprint);
        v.absorb(&Embedding(sample.clone()));
        // Their longest line, as the sample to play.
        let clip = transcript
            .lines
            .iter()
            .filter(|l| l.track == switchscribe_capture::Track::Mic)
            .max_by_key(|l| l.end_ms - l.start_ms)
            .map(|l| {
                (
                    meeting_id.to_owned(),
                    l.start_ms,
                    l.end_ms.min(l.start_ms + 6_000),
                )
            });
        s.set_voiceprint(&StoredVoiceprint {
            person_id: me,
            embedding: v.embedding.0,
            samples: v.samples,
            sample: clip,
        })
        .map_err(|e| e.to_string())?;
    }
    s.set_status(meeting_id, status)
        .map_err(|e| e.to_string())?;
    Ok(status)
}

fn to_voiceprint(v: &StoredVoiceprint) -> Voiceprint {
    Voiceprint {
        person_id: v.person_id,
        embedding: Embedding(v.embedding.clone()),
        samples: v.samples,
    }
}

fn empty_voiceprint(person_id: i64) -> Voiceprint {
    Voiceprint {
        person_id,
        embedding: Embedding(Vec::new()),
        samples: 0,
    }
}

/// Who the user named a speaker as.
#[derive(Clone, Debug)]
enum Who {
    /// Someone in the library.
    Person(i64),
    /// A name typed in; matched to an existing person by name, else added.
    Name(String),
}

/// A speaker naming to write.
struct Naming {
    meeting_id: String,
    cluster: Cluster,
    who: Who,
    remember: bool,
    file: SpeakersFile,
    /// Where to save `speakers.json`; `None` for the demo, which keeps it in memory.
    dir: Option<PathBuf>,
}

/// The library after a change.
struct Library {
    people: Vec<Person>,
    stats: Vec<PersonStats>,
    voiceprints: Vec<StoredVoiceprint>,
    meetings: Vec<Meeting>,
}

fn read_library(s: &Store) -> switchscribe_store::Result<Library> {
    Ok(Library {
        people: s.people()?,
        stats: s.people_stats()?,
        voiceprints: s.voiceprints()?,
        meetings: s.meetings()?,
    })
}

/// Name a speaker: every line with their label, the meeting's attendees, the voiceprint
/// (when remembered) and `speakers.json`. Runs off the UI thread.
fn apply_naming(store: &SharedStore, n: Naming) -> Result<(SpeakersFile, Library), String> {
    let s = store
        .lock()
        .map_err(|_| "the index is unavailable".to_owned())?;
    let err = |e: switchscribe_store::Error| e.to_string();
    let people = s.people().map_err(err)?;
    let person_id = match &n.who {
        Who::Person(id) => *id,
        Who::Name(name) => match people
            .iter()
            .find(|p| p.name.trim().eq_ignore_ascii_case(name.trim()))
        {
            Some(p) => p.id,
            None => {
                let color = next_color(people.iter().map(|p| p.color));
                s.add_person(name.trim(), Some(color), false).map_err(err)?
            }
        },
    };
    s.assign_speaker(&n.meeting_id, &n.cluster.label, Some(person_id))
        .map_err(err)?;
    s.add_attendee(&n.meeting_id, person_id).map_err(err)?;
    if n.remember && !n.cluster.embedding.is_empty() {
        let mut v = s
            .voiceprints()
            .map_err(err)?
            .iter()
            .find(|v| v.person_id == person_id)
            .map_or_else(|| empty_voiceprint(person_id), to_voiceprint);
        v.absorb(&Embedding(n.cluster.embedding.clone()));
        s.set_voiceprint(&StoredVoiceprint {
            person_id,
            embedding: v.embedding.0,
            samples: v.samples,
            sample: n
                .cluster
                .clips
                .first()
                .map(|(a, b)| (n.meeting_id.clone(), *a, *b)),
        })
        .map_err(err)?;
    }
    let mut file = n.file;
    for c in &mut file.clusters {
        if c.label == n.cluster.label {
            c.person_id = Some(person_id);
            c.similarity = None;
            c.confirmed = true;
        }
    }
    if let Some(dir) = &n.dir {
        file.save(dir).map_err(|e| e.to_string())?;
    }
    let status = if file.unnamed().next().is_some() {
        MeetingStatus::NeedsLabels
    } else {
        MeetingStatus::Ready
    };
    s.set_status(&n.meeting_id, status).map_err(err)?;
    Ok((file, read_library(&s).map_err(err)?))
}

/// A voiceprint suggestion's wording: `likely`, `possible`, `unlikely`.
fn likelihood(similarity: f32, t: &Thresholds) -> &'static str {
    if similarity >= t.medium {
        "likely"
    } else if similarity >= t.medium * 0.66 {
        "possible"
    } else {
        "unlikely"
    }
}

impl Workspace {
    /// Read the selected meeting's `speakers.json` on a background thread.
    pub(crate) fn load_speakers(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected().map(|m| m.id.clone()) else {
            return;
        };
        if self.speakers.file.as_ref().is_some_and(|(f, _)| *f == id) {
            return;
        }
        let demo = self.demo;
        if demo {
            self.speakers.file = (id == demo::FEATURED).then(|| (id.clone(), demo::speakers()));
        }
        let dir = self.meeting_dir(&id);
        let store = self.store.clone();
        let meeting = id.clone();
        let task = cx.background_executor().spawn(async move {
            let invitees = store
                .lock()
                .ok()
                .and_then(|s| {
                    let json = s.meeting_calendar(&meeting).ok()??;
                    let people = s.people().ok()?;
                    Some(crate::integrations_view::new_invitees(Some(&json), &people))
                })
                .unwrap_or_default();
            let file = if demo { None } else { SpeakersFile::load(&dir) };
            (file, invitees)
        });
        cx.spawn(async move |this, cx| {
            let (file, invitees) = task.await;
            let _ = this.update(cx, |ws, cx| {
                if !demo {
                    ws.speakers.file = file.map(|f| (id, f));
                }
                ws.speakers.invitees = invitees;
                cx.notify();
            });
        })
        .detach();
    }

    /// Read `speakers.json` again (it was just written).
    pub(crate) fn reload_speakers(&mut self, cx: &mut Context<Self>) {
        if !self.demo {
            self.speakers.file = None;
        }
        self.load_speakers(cx);
    }

    /// The speaker the Name-a-speaker card asks about, if any.
    pub(crate) fn naming_cluster(&self) -> Option<&Cluster> {
        let m = self.selected()?;
        let (id, file) = self.speakers.file.as_ref()?;
        if *id != m.id {
            return None;
        }
        if let Some((rid, label)) = &self.speakers.relabel
            && *rid == m.id
        {
            return file.clusters.iter().find(|c| c.label == *label);
        }
        file.unnamed().find(|c| {
            !self
                .speakers
                .later
                .contains(&(m.id.clone(), c.label.clone()))
        })
    }

    /// Whether a transcript line's speaker can be relabelled (a Teams voice).
    pub(crate) fn can_relabel(&self, label: &str) -> bool {
        self.speakers
            .file
            .as_ref()
            .is_some_and(|(_, f)| f.clusters.iter().any(|c| c.label == label))
    }

    /// Open the card for the speaker of a transcript line.
    pub(crate) fn relabel(&mut self, label: String, cx: &mut Context<Self>) {
        if let Some(m) = self.selected() {
            self.speakers.relabel = Some((m.id.clone(), label));
            self.body_scroll.scroll_to_item(0);
        }
        cx.notify();
    }

    /// Put the card's speaker off until next time.
    fn name_later(&mut self, cx: &mut Context<Self>) {
        if let (Some(m), Some(c)) = (self.selected(), self.naming_cluster()) {
            let key = (m.id.clone(), c.label.clone());
            if self.speakers.relabel.is_some() {
                self.speakers.relabel = None;
            } else {
                self.speakers.later.insert(key);
            }
        }
        cx.notify();
    }

    fn toggle_remember(&mut self, cx: &mut Context<Self>) {
        self.speakers.remember = !self.speakers.remember;
        cx.notify();
    }

    /// Name the card's speaker: as `person`, or as the typed name when `None`.
    fn name_speaker(&mut self, person: Option<i64>, window: &mut Window, cx: &mut Context<Self>) {
        if self.speakers.busy {
            return;
        }
        let who = match person {
            Some(id) => Who::Person(id),
            None => {
                let typed = self.speakers.name_input.read(cx).value().trim().to_owned();
                if typed.is_empty() {
                    return;
                }
                Who::Name(typed)
            }
        };
        self.name_as(who, window, cx);
    }

    /// Name the speaker as a calendar invitee (a new person in the library).
    fn name_invitee(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.speakers.busy {
            self.name_as(Who::Name(name), window, cx);
        }
    }

    fn name_as(&mut self, who: Who, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(m), Some(cluster)) = (self.selected(), self.naming_cluster().cloned()) else {
            return;
        };
        let Some((_, file)) = self.speakers.file.clone() else {
            return;
        };
        let meeting_id = m.id.clone();
        let naming = Naming {
            dir: (!self.demo).then(|| self.meeting_dir(&meeting_id)),
            meeting_id: meeting_id.clone(),
            cluster,
            who,
            remember: self.speakers.remember,
            file,
        };
        self.speakers.busy = true;
        self.speakers
            .name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        let store = self.store.clone();
        let task = cx
            .background_executor()
            .spawn(async move { apply_naming(&store, naming) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |ws, cx| {
                ws.speakers.busy = false;
                ws.speakers.relabel = None;
                match result {
                    Ok((file, library)) => {
                        ws.speakers.file = Some((meeting_id, file));
                        ws.take_library(library);
                        ws.reload_transcript(cx);
                    }
                    Err(e) => ws
                        .banners
                        .push(Banner::plain(format!("Could not name the speaker: {e}"))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn take_library(&mut self, l: Library) {
        self.people = l.people;
        self.speakers.stats = l.stats;
        self.speakers.voiceprints = l.voiceprints;
        self.meetings = l.meetings;
    }

    /// Read the library again (identification changed it).
    pub(crate) fn refresh_library(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let s = store
                .lock()
                .map_err(|_| "the index is unavailable".to_owned())?;
            read_library(&s).map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |ws, cx| {
                match result {
                    Ok(l) => ws.take_library(l),
                    Err(e) => tracing::warn!(error = %e, "could not read the people"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Play a clip of the selected meeting and pause at its end.
    pub(crate) fn play_clip(&mut self, start_ms: i64, end_ms: i64, cx: &mut Context<Self>) {
        let Some(pb) = &mut self.playback else {
            return;
        };
        pb.seek_ms(start_ms.max(0) as u64);
        pb.play();
        self.playhead = start_ms / 1000;
        self.speakers.clip_end = Some(end_ms.max(start_ms) as u64);
        self.follow_playback(cx);
        cx.notify();
    }

    /// Play a person's voiceprint sample: open its meeting, then play the clip.
    fn play_sample(&mut self, sample: (String, i64, i64), cx: &mut Context<Self>) {
        let (id, start, end) = sample;
        if !self.meetings.iter().any(|m| m.id == id) {
            return;
        }
        if self.selected().is_some_and(|m| m.id == id) && self.playback.is_some() {
            self.screen = Screen::Meeting(id);
            self.play_clip(start, end, cx);
            return;
        }
        self.speakers.pending_clip = Some((id.clone(), start, end));
        self.select(id, cx);
    }

    /// The meeting's audio just loaded: play the clip waiting for it.
    pub(crate) fn start_pending_clip(&mut self, cx: &mut Context<Self>) {
        let Some((id, start, end)) = self.speakers.pending_clip.take() else {
            return;
        };
        if self.selected().is_some_and(|m| m.id == id) {
            self.play_clip(start, end, cx);
        }
    }

    /// Forget a person's voice; they keep their name in past meetings.
    fn forget_voice(&mut self, person_id: i64, cx: &mut Context<Self>) {
        self.library_change(cx, move |s| {
            s.delete_voiceprint(person_id).map_err(|e| e.to_string())
        });
    }

    /// Fold the person being merged into `into`: their lines, meetings and voiceprint.
    fn merge_into(&mut self, into: i64, cx: &mut Context<Self>) {
        let Some(from) = self.speakers.merging.take() else {
            return;
        };
        self.library_change(cx, move |s| {
            let err = |e: switchscribe_store::Error| e.to_string();
            let prints = s.voiceprints().map_err(err)?;
            let a = prints.iter().find(|v| v.person_id == from);
            let b = prints.iter().find(|v| v.person_id == into);
            let merged = merge_voiceprints(into, a, b);
            s.merge_people(from, into).map_err(err)?;
            if let Some(v) = merged {
                s.set_voiceprint(&v).map_err(err)?;
            }
            Ok(())
        });
    }

    /// Run a library change off the UI thread, then show the library again.
    fn library_change(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Store) -> Result<(), String> + Send + 'static,
    ) {
        if self.speakers.busy {
            return;
        }
        self.speakers.busy = true;
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let mut s = store
                .lock()
                .map_err(|_| "the index is unavailable".to_owned())?;
            change(&mut s)?;
            read_library(&s).map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |ws, cx| {
                ws.speakers.busy = false;
                match result {
                    Ok(l) => {
                        ws.take_library(l);
                        ws.reload_transcript(cx);
                    }
                    Err(e) => ws
                        .banners
                        .push(Banner::plain(format!("Could not change the library: {e}"))),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// Two voiceprints as one, weighted by their sample counts; `None` when neither exists.
fn merge_voiceprints(
    into: i64,
    a: Option<&StoredVoiceprint>,
    b: Option<&StoredVoiceprint>,
) -> Option<StoredVoiceprint> {
    match (a, b) {
        (None, None) => None,
        (Some(v), None) | (None, Some(v)) => Some(StoredVoiceprint {
            person_id: into,
            ..v.clone()
        }),
        (Some(a), Some(b)) if a.embedding.len() == b.embedding.len() => {
            let (na, nb) = (a.samples.max(1) as f32, b.samples.max(1) as f32);
            Some(StoredVoiceprint {
                person_id: into,
                embedding: a
                    .embedding
                    .iter()
                    .zip(&b.embedding)
                    .map(|(x, y)| (x * na + y * nb) / (na + nb))
                    .collect(),
                samples: a.samples + b.samples,
                sample: b.sample.clone().or_else(|| a.sample.clone()),
            })
        }
        // Different models: keep the one being merged into.
        (Some(_), Some(b)) => Some(StoredVoiceprint {
            person_id: into,
            ..b.clone()
        }),
    }
}

/// A checkbox: a 14 px box, ticked when `on`.
/// The Name-a-speaker card: clips to listen to, voice-matched suggestions, a name field.
pub fn name_card(
    ws: &Workspace,
    cluster: &Cluster,
    transcript: &[Segment],
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let lines = transcript
        .iter()
        .filter(|s| s.speaker_label == cluster.label)
        .count();
    let thresholds = Thresholds::default();
    let me = ws.people.iter().find(|p| p.is_me).map(|p| p.id);
    let attendees: Vec<i64> = ws
        .selected()
        .map(|m| m.attendees.iter().map(|a| a.id).collect())
        .unwrap_or_default();
    // Voice matches first; without a voiceprint, the attendees nobody is named as yet.
    let library: Vec<Voiceprint> = ws
        .speakers
        .voiceprints
        .iter()
        .filter(|v| Some(v.person_id) != me)
        .map(to_voiceprint)
        .collect();
    let mut suggestions: Vec<(i64, Option<f32>)> = if cluster.embedding.is_empty() {
        Vec::new()
    } else {
        rank(&Embedding(cluster.embedding.clone()), &library, &attendees)
            .into_iter()
            .take(3)
            .map(|(id, s)| (id, Some(s)))
            .collect()
    };
    if suggestions.is_empty() {
        let named: Vec<i64> = ws
            .speakers
            .file
            .as_ref()
            .map(|(_, f)| {
                f.clusters
                    .iter()
                    .filter(|c| c.confirmed)
                    .filter_map(|c| c.person_id)
                    .collect()
            })
            .unwrap_or_default();
        suggestions = attendees
            .iter()
            .filter(|a| Some(**a) != me && !named.contains(a))
            .take(3)
            .map(|a| (*a, None))
            .collect();
    }
    let has_audio = ws.playback.as_ref().is_some_and(|pb| pb.has_audio());
    let clips = cluster.clips.iter().enumerate().map(|(i, &(a, b))| {
        div()
            .id(("clip", i))
            .flex()
            .items_center()
            .gap(rpx(8.))
            .py(rpx(6.))
            .pl(rpx(6.))
            .pr(rpx(10.))
            .rounded(px(6.))
            .border_1()
            .border_color(p.bd2)
            .bg(p.bg)
            .when(has_audio, |d| {
                d.cursor_pointer()
                    .hover(|s| s.border_color(p.bd3))
                    .on_click(cx.listener(move |ws, _, _, cx| ws.play_clip(a, b, cx)))
            })
            .when(!has_audio, |d| d.opacity(0.6))
            .child(
                div()
                    .size(rpx(22.))
                    .rounded_full()
                    .bg(p.bd2)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(ts::TINY)
                    .text_color(p.fg)
                    .child("▶"),
            )
            .child(ui::mono(format::clock(a / 1000), ts::SMALL, p.fg2))
            .child(ui::mono(format::seconds(b - a), ts::SMALL, p.fg3))
    });
    let chips = suggestions
        .into_iter()
        .enumerate()
        .filter_map(|(i, (id, sim))| {
            let person = ws.person(id)?;
            let likely = sim.is_some_and(|s| s >= thresholds.medium);
            let color = p.speaker(person.color);
            let (detail, detail_color) = match sim {
                Some(s) => (
                    format!(
                        "{:.0}% · {}",
                        (s.max(0.0) * 100.0),
                        likelihood(s, &thresholds)
                    ),
                    if likely {
                        color
                    } else if s >= thresholds.medium * 0.66 {
                        p.fg2
                    } else {
                        p.fg3
                    },
                ),
                None => ("attendee".to_owned(), p.fg3),
            };
            Some(
                div()
                    .id(("suggest", i))
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(10.))
                    .py(rpx(5.))
                    .rounded(px(6.))
                    .border_1()
                    .border_dashed()
                    .border_color(if likely { color } else { p.fg4 })
                    .cursor_pointer()
                    .hover(|s| s.bg(p.chip))
                    .on_click(
                        cx.listener(move |ws, _, window, cx| ws.name_speaker(Some(id), window, cx)),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.fg)
                            .child(format!("{}?", person.name)),
                    )
                    .child(ui::mono(detail, ts::SMALL, detail_color)),
            )
        });
    let invitees = ws
        .speakers
        .invitees
        .iter()
        .take(4)
        .enumerate()
        .map(|(i, name)| {
            let name = name.clone();
            div()
                .id(("invitee", i))
                .flex()
                .items_center()
                .gap(rpx(8.))
                .px(rpx(10.))
                .py(rpx(5.))
                .rounded(px(6.))
                .border_1()
                .border_dashed()
                .border_color(p.fg4)
                .cursor_pointer()
                .hover(|s| s.bg(p.chip))
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.fg)
                        .child(format!("{name}?")),
                )
                .child(ui::mono("invited", ts::SMALL, p.fg3))
                .on_click(
                    cx.listener(move |ws, _, window, cx| ws.name_invitee(name.clone(), window, cx)),
                )
        });
    let remember = ws.speakers.remember;
    let relabeling = ws.speakers.relabel.is_some();
    div()
        .mx(rpx(8.))
        .mt(rpx(8.))
        .mb(rpx(4.))
        .p(rpx(14.))
        .max_w(rpx(760.))
        .flex()
        .flex_col()
        .gap(rpx(12.))
        .border_1()
        .border_color(p.bd3)
        .rounded(px(8.))
        .bg(p.surface)
        .text_size(ts::BODY)
        .child(
            div()
                .flex()
                .items_center()
                .gap(rpx(10.))
                .child(ui::dot(
                    10.,
                    match cluster.person_id.and_then(|id| ws.person(id)) {
                        Some(person) if cluster.confirmed => p.speaker(person.color),
                        _ => p.unknown,
                    },
                ))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(ts::UI)
                        .text_color(p.fg)
                        .child(format!("Who is {}?", cluster.label)),
                )
                .child(div().text_color(p.fg3).child(format!(
                    "{lines} {} · {} of speech",
                    if lines == 1 { "segment" } else { "segments" },
                    format::seconds(cluster.speech_ms)
                )))
                .child(div().flex_1())
                .child(
                    div()
                        .id("name-later")
                        .text_color(p.fg3)
                        .cursor_pointer()
                        .hover(|s| s.text_color(p.fg))
                        .on_click(cx.listener(|ws, _, _, cx| ws.name_later(cx)))
                        .child(if relabeling { "Cancel" } else { "Later" }),
                ),
        )
        .when(!cluster.clips.is_empty(), |d| {
            d.child(div().flex().gap(rpx(8.)).children(clips))
        })
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(rpx(8.))
                .children(chips)
                .children(invitees)
                .child(
                    div()
                        .w(rpx(170.))
                        .child(Input::new(&ws.speakers.name_input).text_size(ts::BODY)),
                )
                .child(
                    div()
                        .id("remember")
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .text_color(p.fg_ctl)
                        .cursor_pointer()
                        .on_click(cx.listener(|ws, _, _, cx| ws.toggle_remember(cx)))
                        .child(ui::checkbox(remember, p))
                        .child("Remember this voice"),
                )
                .child(div().flex_1())
                .child(
                    ui::button("name-confirm", "Confirm", Kind::Primary, p).on_click(
                        cx.listener(|ws, _, window, cx| ws.name_speaker(None, window, cx)),
                    ),
                ),
        )
        .into_any_element()
}

/// The People screen: everyone heard, with their voiceprints.
pub fn people_screen(ws: &Workspace, p: &Palette, cx: &mut Context<Workspace>) -> AnyElement {
    const NAME: f32 = 0.0;
    const MEETINGS: f32 = 110.;
    const SAMPLES: f32 = 90.;
    const LAST: f32 = 110.;
    const ACTIONS: f32 = 280.;
    let cell = |w: f32| {
        let d = div().px(rpx(4.)).flex().items_center();
        if w == NAME {
            d.flex_1().min_w_0()
        } else {
            d.w(rpx(w)).flex_none()
        }
    };
    let header = div()
        .flex()
        .py(rpx(8.))
        .border_b_1()
        .border_color(p.bd)
        .text_size(ts::SMALL)
        .text_color(p.fg3)
        .child(cell(NAME).child("Name"))
        .child(cell(MEETINGS).child("Meetings heard"))
        .child(cell(SAMPLES).child("Samples"))
        .child(cell(LAST).child("Last heard"))
        .child(cell(ACTIONS));
    let action = |id: (&'static str, usize), label: &'static str, color| {
        div()
            .id(id)
            .px(rpx(8.))
            .py(rpx(3.))
            .rounded(px(5.))
            .border_1()
            .border_color(p.bd2)
            .text_size(ts::SMALL)
            .text_color(color)
            .cursor_pointer()
            .hover(|s| s.bg(p.chip))
            .child(label)
    };
    let merging = ws.speakers.merging;
    let rows = ws.people.iter().enumerate().map(|(i, person)| {
        let stats = ws
            .speakers
            .stats
            .iter()
            .find(|s| s.person_id == person.id)
            .cloned()
            .unwrap_or_default();
        let pid = person.id;
        let samples = match (stats.samples, person.is_me) {
            (0, _) => "—".to_owned(),
            (n, true) => format!("{n} enrolled"),
            (n, false) => n.to_string(),
        };
        let actions: AnyElement = if merging == Some(pid) {
            div()
                .flex()
                .flex_wrap()
                .justify_end()
                .items_center()
                .gap(rpx(4.))
                .child(
                    div()
                        .text_size(ts::SMALL)
                        .text_color(p.fg3)
                        .child("Merge into"),
                )
                .children(
                    ws.people
                        .iter()
                        .filter(|o| o.id != pid)
                        .enumerate()
                        .map(|(j, o)| {
                            let into = o.id;
                            div()
                                .id(("merge-into", j))
                                .flex()
                                .items_center()
                                .gap(rpx(5.))
                                .px(rpx(8.))
                                .py(rpx(3.))
                                .rounded(px(5.))
                                .border_1()
                                .border_color(p.bd2)
                                .text_size(ts::SMALL)
                                .text_color(p.fg_ctl)
                                .cursor_pointer()
                                .hover(|s| s.bg(p.chip))
                                .on_click(cx.listener(move |ws, _, _, cx| ws.merge_into(into, cx)))
                                .child(ui::dot(7., p.speaker(o.color)))
                                .child(o.name.clone())
                        }),
                )
                .child(
                    action(("merge-cancel", i), "Cancel", p.fg3).on_click(cx.listener(
                        |ws, _, _, cx| {
                            ws.speakers.merging = None;
                            cx.notify();
                        },
                    )),
                )
                .into_any_element()
        } else {
            let sample = stats.sample.clone();
            div()
                .flex()
                .justify_end()
                .items_center()
                .gap(rpx(4.))
                .when_some(sample, |d, sample| {
                    d.child(
                        action(("play-sample", i), "Play sample", p.fg_ctl).on_click(
                            cx.listener(move |ws, _, _, cx| ws.play_sample(sample.clone(), cx)),
                        ),
                    )
                })
                .when(!person.is_me, |d| {
                    d.child(
                        action(("merge", i), "Merge", p.fg_ctl).on_click(cx.listener(
                            move |ws, _, _, cx| {
                                ws.speakers.merging = Some(pid);
                                cx.notify();
                            },
                        )),
                    )
                })
                .when(stats.samples > 0, |d| {
                    d.child(
                        action(("forget", i), "Forget voice", p.fg2)
                            .on_click(cx.listener(move |ws, _, _, cx| ws.forget_voice(pid, cx))),
                    )
                })
                .into_any_element()
        };
        div()
            .flex()
            .py(rpx(6.))
            .min_h(rpx(40.))
            .border_b_1()
            .border_color(p.elev)
            .when(person.is_me, |d| d.bg(p.surface))
            .child(
                cell(NAME)
                    .gap(rpx(10.))
                    .child(
                        div()
                            .size(rpx(12.))
                            .flex_none()
                            .rounded(px(3.))
                            .bg(p.speaker(person.color)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::MEDIUM)
                            .child(person.name.clone()),
                    )
                    .when(person.is_me, |d| {
                        d.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child("you · mic track"),
                        )
                    }),
            )
            .child(cell(MEETINGS).child(ui::mono(stats.meetings.to_string(), ts::BODY, p.fg_ctl)))
            .child(cell(SAMPLES).child(ui::mono(samples, ts::BODY, p.fg_ctl)))
            .child(
                cell(LAST).text_color(p.fg2).child(
                    stats
                        .last_heard
                        .map_or_else(|| "—".to_owned(), |t| format::day(t, ws.today)),
                ),
            )
            .child(cell(ACTIONS).justify_end().child(actions))
    });
    let note = if cfg!(windows) {
        "Voiceprints are stored encrypted on this machine and can be deleted per person."
    } else {
        "Voiceprints are stored on this machine and can be deleted per person."
    };
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(crate::workspace::screen_title("People", Some(note), p))
        .child(if ws.people.is_empty() {
            ui::empty_state(
                "No one yet",
                "People are added when you name a speaker in a transcript.",
                p,
            )
            .into_any_element()
        } else {
            div()
                .id("people")
                .flex_1()
                .overflow_y_scroll()
                .px(rpx(20.))
                .pb(rpx(20.))
                .text_size(ts::UI)
                .child(header)
                .children(rows)
                .into_any_element()
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp(id: i64, e: Vec<f32>, samples: u32) -> StoredVoiceprint {
        StoredVoiceprint {
            person_id: id,
            embedding: e,
            samples,
            sample: None,
        }
    }

    #[test]
    fn merging_voiceprints_weights_by_samples() {
        let a = vp(1, vec![1.0, 0.0], 3);
        let b = vp(2, vec![0.0, 1.0], 1);
        let m = merge_voiceprints(2, Some(&a), Some(&b)).unwrap();
        assert_eq!(m.person_id, 2);
        assert_eq!(m.samples, 4);
        assert_eq!(m.embedding, vec![0.75, 0.25]);
        assert_eq!(merge_voiceprints(2, Some(&a), None).unwrap().person_id, 2);
        assert!(merge_voiceprints(2, None, None).is_none());
        // Different lengths: the target's survives.
        let c = vp(2, vec![0.5; 3], 2);
        assert_eq!(
            merge_voiceprints(2, Some(&a), Some(&c))
                .unwrap()
                .embedding
                .len(),
            3
        );
    }

    #[test]
    fn the_user_is_added_once() {
        let store = Store::open_in_memory().unwrap();
        ensure_me(&store).unwrap();
        ensure_me(&store).unwrap();
        let people = store.people().unwrap();
        assert_eq!(people.len(), 1);
        assert!(people[0].is_me && !people[0].name.is_empty());
    }

    #[test]
    fn naming_a_speaker_updates_everything() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .insert_meeting(&switchscribe_store::NewMeeting {
                id: "m".into(),
                title: "t".into(),
                started_at: 0,
                duration_secs: 60,
                source_app: "Teams".into(),
                status: MeetingStatus::NeedsLabels,
                attendees: vec![],
            })
            .unwrap();
        store
            .add_segments(
                "m",
                &[switchscribe_store::NewSegment {
                    start_ms: 0,
                    end_ms: 1000,
                    person_id: None,
                    speaker_label: "Speaker 1".into(),
                    text: "hello".into(),
                    low_confidence: false,
                }],
            )
            .unwrap();
        let cluster = Cluster {
            label: "Speaker 1".into(),
            embedding: vec![1.0, 0.0],
            speech_ms: 1000,
            clips: vec![(0, 1000)],
            person_id: None,
            similarity: None,
            confirmed: false,
        };
        let store = std::sync::Arc::new(std::sync::Mutex::new(store));
        let dir = tempfile::tempdir().unwrap();
        let (file, lib) = apply_naming(
            &store,
            Naming {
                meeting_id: "m".into(),
                cluster: cluster.clone(),
                who: Who::Name("Dana Ortiz".into()),
                remember: true,
                file: SpeakersFile {
                    clusters: vec![cluster],
                    me: None,
                },
                dir: Some(dir.path().to_path_buf()),
            },
        )
        .unwrap();
        let dana = lib.people.iter().find(|p| p.name == "Dana Ortiz").unwrap();
        assert!(file.clusters[0].confirmed);
        assert_eq!(file.clusters[0].person_id, Some(dana.id));
        assert_eq!(SpeakersFile::load(dir.path()), Some(file));
        assert_eq!(lib.voiceprints.len(), 1);
        assert_eq!(lib.voiceprints[0].sample, Some(("m".into(), 0, 1000)));
        let m = lib.meetings.iter().find(|m| m.id == "m").unwrap();
        assert_eq!(m.status, MeetingStatus::Ready);
        assert_eq!(m.attendees.len(), 1);
        let s = store.lock().unwrap();
        assert_eq!(s.segments("m").unwrap()[0].person_id, Some(dana.id));
    }

    /// The whole path on sherpa-onnx's four-speaker sample, played as the Teams track:
    /// identify, name a speaker, identify again and recognize them. Needs the models
    /// (and `four.wav`) in `SWITCHSCRIBE_TEST_MODELS`.
    #[test]
    fn identifies_names_and_recognizes_on_the_sample() {
        use switchscribe_capture::Track;
        use switchscribe_capture::ogg_opus::OpusWriter;
        use switchscribe_pipeline::job::{TRANSCRIPT_FILE, TranscriptLine};

        let Some(models) = std::env::var_os("SWITCHSCRIBE_TEST_MODELS") else {
            return;
        };
        let models = Path::new(&models);
        let wav = std::fs::read(models.join("four.wav")).unwrap();
        // 16-bit mono PCM after a 44-byte header, 16 kHz; the writer takes 48 kHz.
        let pcm: Vec<f32> = wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| f32::from(i16::from_le_bytes(*b)) / 32768.0)
            .flat_map(|x| [x, x, x])
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let mut w = OpusWriter::create(&dir.path().join(Track::Remote.file_name())).unwrap();
        w.push(&pcm).unwrap();
        w.finish().unwrap();
        let ms = (pcm.len() / 48) as i64;
        let lines: Vec<TranscriptLine> = (0..ms / 2_000)
            .map(|i| TranscriptLine {
                start_ms: i * 2_000,
                end_ms: i * 2_000 + 1_900,
                track: Track::Remote,
                text: format!("line {i}"),
            })
            .collect();
        let transcript = TranscriptFile {
            language: None,
            lines,
        };
        std::fs::write(
            dir.path().join(TRANSCRIPT_FILE),
            serde_json::to_string(&transcript).unwrap(),
        )
        .unwrap();

        let mut store = Store::open_in_memory().unwrap();
        ensure_me(&store).unwrap();
        store
            .insert_meeting(&switchscribe_store::NewMeeting {
                id: "m".into(),
                title: "t".into(),
                started_at: 0,
                duration_secs: (ms / 1000) as u32,
                source_app: "Teams".into(),
                status: MeetingStatus::Processing,
                attendees: vec![],
            })
            .unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(store));
        let cancel = AtomicBool::new(false);
        let status = identify_meeting(dir.path(), models, &store, "m", &|_| {}, &cancel).unwrap();
        assert_eq!(status, MeetingStatus::NeedsLabels);
        let file = SpeakersFile::load(dir.path()).unwrap();
        assert!(file.clusters.len() >= 2, "{} clusters", file.clusters.len());
        let first = file.clusters[0].clone();
        assert!(!first.embedding.is_empty() && !first.clips.is_empty());

        let (_, lib) = apply_naming(
            &store,
            Naming {
                meeting_id: "m".into(),
                cluster: first.clone(),
                who: Who::Name("Alex".into()),
                remember: true,
                file,
                dir: Some(dir.path().to_path_buf()),
            },
        )
        .unwrap();
        let alex = lib.people.iter().find(|p| p.name == "Alex").unwrap().id;

        identify_meeting(dir.path(), models, &store, "m", &|_| {}, &cancel).unwrap();
        let again = SpeakersFile::load(dir.path()).unwrap();
        let c = again
            .clusters
            .iter()
            .find(|c| c.label == first.label)
            .unwrap();
        assert_eq!(c.person_id, Some(alex), "{again:?}");
        assert!(c.confirmed, "similarity {:?}", c.similarity);
        let s = store.lock().unwrap();
        assert!(
            s.segments("m")
                .unwrap()
                .iter()
                .any(|x| x.person_id == Some(alex))
        );
    }
}
