//! The main window: title bar, meetings sidebar (260 px), the main pane and the Ask panel
//! (360 px), laid out like the prototype. The sidebar and the Ask panel collapse.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate};
use gpui_kit::component::TitleBar;
use gpui_kit::component::input::{Input, InputEvent, InputState, Position, TextareaState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AnyWindowHandle, AppContext as _, Context, Entity, FocusHandle, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px, svg,
};
use switchscribe_capture::{Backend, InputDevice, Recorder, RecorderState, RemoteMode};
use switchscribe_store::{
    ActionStatus, CaptureMode, Meeting, MeetingStatus, NewMeeting, Paths, Person, PersonStats,
    SearchFilter, SearchHit, Segment, Settings, Store, StoredVoiceprint, group_by_date,
};

use crate::actions;
use crate::agent_view::{self, Agent};
use crate::appearance::{rpx, ts};
use crate::ask_panel::{AskEvent, AskPanel, AskRequest, Scope};
use crate::player_bar::Playback;
use crate::processing::Jobs;
use crate::recording::{self, Banner, NotesDoc, Session};
use crate::speakers::{self, Speakers};
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind};
use crate::{live_view, meeting_view, mini_recorder};

/// The index, shared with background tasks.
pub type SharedStore = Arc<Mutex<Store>>;

/// What the app read at startup.
pub struct Startup {
    /// Data folder.
    pub paths: Paths,
    /// The settings file's contents.
    pub settings: Settings,
    /// The index (in memory for the demo, or when the file could not be opened).
    pub store: SharedStore,
    /// Every meeting, newest first.
    pub meetings: Vec<Meeting>,
    /// Everyone in the voiceprint library.
    pub people: Vec<Person>,
    /// Showing the demo data.
    pub demo: bool,
    /// Why the database could not be opened, if it could not.
    pub store_error: Option<String>,
    /// Microphones, default first (empty where capture is unsupported).
    pub devices: Vec<InputDevice>,
    /// Models in the models folder, by id.
    pub installed_models: HashSet<&'static str>,
    /// The voiceprint library.
    pub voiceprints: Vec<StoredVoiceprint>,
    /// Per-person numbers for the People screen.
    pub people_stats: Vec<PersonStats>,
    /// Requests from later launches (`None` in the demo).
    pub listener: Option<crate::instance::Listener>,
    /// What this launch was asked to do (a link to a moment).
    pub request: crate::instance::Request,
}

/// The main pane's screens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Screen {
    /// A meeting, by id.
    Meeting(String),
    /// The recording in progress.
    Live,
    /// Nothing recorded yet.
    Empty,
    /// Cross-meeting action items.
    ActionItems,
    /// The voiceprint library.
    People,
    /// Search all meetings.
    Search,
    /// Settings.
    Settings,
}

/// A meeting's tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    /// Summary, decisions, action items, open questions.
    Summary,
    /// The transcript.
    Transcript,
    /// This meeting's action items.
    Actions,
    /// The user's notes.
    Notes,
}

/// The root view.
pub struct Workspace {
    pub(crate) paths: Paths,
    pub(crate) settings: Settings,
    pub(crate) store: SharedStore,
    pub(crate) demo: bool,
    store_error: Option<String>,
    pub(crate) meetings: Vec<Meeting>,
    pub(crate) people: Vec<Person>,
    pub(crate) today: NaiveDate,
    pub(crate) screen: Screen,
    pub(crate) tab: Tab,
    /// The loaded transcript: meeting id and its segments.
    pub(crate) transcript: Option<(String, Vec<Segment>)>,
    /// The cued moment of the selected meeting, seconds.
    pub(crate) playhead: i64,
    /// The selected meeting's player.
    pub(crate) playback: Option<Playback>,
    /// Scrolls the meeting body (the transcript follows playback).
    pub(crate) body_scroll: ScrollHandle,
    /// Transcription and model downloads.
    pub(crate) jobs: Jobs,
    /// The playhead follows playback.
    pub(crate) following: bool,
    /// Speaker files, the library, and the naming card's state.
    pub(crate) speakers: Speakers,
    /// Summaries, action items and the CLI.
    pub(crate) agent: Agent,
    /// Meeting detection.
    pub(crate) detect: crate::detection::Detect,
    /// Requests from later launches.
    listener: Option<crate::instance::Listener>,
    /// Exports, Jira and the calendar.
    pub(crate) integ: crate::integrations_view::Integrations,
    ask: Entity<AskPanel>,
    ask_open: bool,
    search_input: Entity<InputState>,
    search_hits: Vec<SearchHit>,
    search_generation: u64,
    /// Search filters: one person, and the last N days.
    search_person: Option<i64>,
    search_days: Option<u32>,
    /// The recording in progress.
    pub(crate) recording: Option<Session>,
    /// A recording is being set up.
    pub(crate) starting: bool,
    /// Amber notices above the main pane (capture fallbacks, lost devices).
    pub(crate) banners: Vec<Banner>,
    /// The selected meeting's notes and bookmarks, read from its folder.
    pub(crate) notes_doc: Option<(String, NotesDoc)>,
    devices: Vec<InputDevice>,
    /// This window, for bringing it forward from the mini recorder and the tray.
    pub(crate) window: AnyWindowHandle,
    mini: Option<AnyWindowHandle>,
    /// First-run setup, while it shows.
    pub(crate) onboarding: Option<crate::onboarding::Onboarding>,
    /// Newer versions.
    pub(crate) updates: crate::updates::Updates,
    #[cfg(windows)]
    tray: Option<crate::tray::Tray>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl Workspace {
    /// The window's root view.
    pub fn new(s: Startup, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let ask =
            cx.new(|cx| AskPanel::new(s.settings.agent.cli, s.demo, s.store.clone(), window, cx));
        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search every meeting"));
        let subs = vec![
            // Quitting mid-call finishes the files rather than cutting them off.
            cx.on_app_quit(|this, cx| {
                this.finish_on_quit(cx);
                async {}
            }),
            cx.subscribe_in(&ask, window, |this, _, ev: &AskEvent, _, cx| match ev {
                AskEvent::Cite(meeting, at) => this.open_moment(meeting, *at, cx),
                AskEvent::Ask(q) => this.answer(q.clone(), cx),
                AskEvent::CreateJira(payload) => this.create_proposed_issue(payload.clone(), cx),
                AskEvent::Changed => {
                    this.load_summary(cx);
                    this.load_all_actions(cx);
                }
            }),
            cx.subscribe_in(&search_input, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    this.run_search(cx);
                }
            }),
        ];
        // The Ask panel opens on wide windows until the user decides.
        let ask_open = s
            .settings
            .layout
            .ask_open
            .unwrap_or(window.viewport_size().width >= px(1200.));
        let screen = match s.meetings.first() {
            Some(_) if s.demo => Screen::Meeting(crate::demo::FEATURED.into()),
            Some(m) => Screen::Meeting(m.id.clone()),
            None => Screen::Empty,
        };
        let integ =
            crate::integrations_view::Integrations::new(&s.settings.integrations, window, cx);
        let onboarding = crate::onboarding::wanted(s.demo, s.settings.onboarded).then(|| {
            let me = s
                .people
                .iter()
                .find(|p| p.is_me)
                .map_or("", |p| p.name.as_str());
            crate::onboarding::Onboarding::new(me, window, cx)
        });
        let mut this = Self {
            paths: s.paths,
            settings: s.settings,
            store: s.store,
            demo: s.demo,
            store_error: s.store_error,
            meetings: s.meetings,
            people: s.people,
            today: Local::now().date_naive(),
            screen,
            tab: Tab::Summary,
            transcript: None,
            playhead: 0,
            playback: None,
            body_scroll: ScrollHandle::new(),
            following: false,
            jobs: Jobs::new(s.installed_models),
            speakers: Speakers::new(s.voiceprints, s.people_stats, window, cx),
            agent: Agent::default(),
            detect: Default::default(),
            listener: s.listener,
            integ,
            ask,
            ask_open,
            search_input,
            search_hits: Vec::new(),
            search_generation: 0,
            search_person: None,
            search_days: None,
            recording: None,
            starting: false,
            banners: Vec::new(),
            notes_doc: None,
            devices: s.devices,
            window: window.window_handle(),
            mini: None,
            onboarding,
            updates: Default::default(),
            #[cfg(windows)]
            tray: crate::tray::Tray::new(),
            focus: cx.focus_handle(),
            _subs: subs,
        };
        if this.demo {
            this.playhead = 236;
        }
        this.focus.focus(window, cx);
        this.load_transcript(cx);
        this.load_speakers(cx);
        this.load_summary(cx);
        this.load_all_actions(cx);
        this.detect_cli(cx);
        this.schedule_update_checks(cx);
        this.prune_audio(cx);
        this.sync_detection(cx);
        this.load_integrations(cx);
        this.handle_request(s.request, window, cx);
        this.poll_instance(window, cx);
        this.load_notes(cx);
        this.load_playback(cx);
        // Meetings left mid-processing (a quit, a crash) pick up where they were.
        if !this.demo {
            let waiting: Vec<String> = this
                .meetings
                .iter()
                .rev()
                .filter(|m| m.status == MeetingStatus::Processing)
                .map(|m| m.id.clone())
                .collect();
            for id in waiting {
                this.queue_transcription(id, cx);
            }
        }
        #[cfg(windows)]
        this.poll_tray(window, cx);
        this
    }

    /// Act on a launch's request: a link opens its moment, anything but a background
    /// start brings the window up.
    fn handle_request(
        &mut self,
        request: crate::instance::Request,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let crate::instance::Request::Open(id, secs) = &request {
            if self.meetings.iter().any(|m| &m.id == id) {
                self.open_moment(id, *secs, cx);
            } else {
                self.banners.push(Banner::plain(
                    "That link points to a meeting that is not here.",
                ));
            }
        }
        if request == crate::instance::Request::Background {
            return;
        }
        window.activate_window();
        cx.notify();
    }

    fn poll_instance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.listener.is_none() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let ok = this.update_in(cx, |ws, window, cx| {
                    let requests = ws.listener.as_ref().map(|l| l.take()).unwrap_or_default();
                    for r in requests {
                        ws.handle_request(r, window, cx);
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// A line in the Ask panel from switchscribe itself.
    pub(crate) fn ask_say(&mut self, text: String, failed: bool, cx: &mut Context<Self>) {
        self.ask.update(cx, |panel, cx| panel.say(text, failed, cx));
    }

    /// The selected meeting.
    pub(crate) fn selected(&self) -> Option<&Meeting> {
        match &self.screen {
            Screen::Meeting(id) => self.meetings.iter().find(|m| &m.id == id),
            _ => None,
        }
    }

    /// A person by id.
    pub(crate) fn person(&self, id: i64) -> Option<&Person> {
        self.people.iter().find(|p| p.id == id)
    }

    pub(crate) fn select(&mut self, id: String, cx: &mut Context<Self>) {
        if self.recording.as_ref().is_some_and(|s| s.meeting_id == id) {
            self.screen = Screen::Live;
        } else if self.screen != Screen::Meeting(id.clone()) {
            self.screen = Screen::Meeting(id);
            self.playhead = 0;
            self.load_transcript(cx);
            self.load_speakers(cx);
            self.load_summary(cx);
            self.load_notes(cx);
            self.load_playback(cx);
        }
        cx.notify();
    }

    /// A meeting's folder. Demo recordings go to a temporary folder so the demo never
    /// touches the data folder.
    pub(crate) fn meeting_dir(&self, id: &str) -> PathBuf {
        if self.demo {
            std::env::temp_dir().join("switchscribe-demo").join(id)
        } else {
            self.paths.meeting_dir(id)
        }
    }

    /// Read the selected meeting's notes and bookmarks on a background thread.
    fn load_notes(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected().map(|m| m.id.clone()) else {
            return;
        };
        let dir = self.meeting_dir(&id);
        let task = cx
            .background_executor()
            .spawn(async move { NotesDoc::load(&dir) });
        cx.spawn(async move |this, cx| {
            let doc = task.await;
            let _ = this.update(cx, |ws, cx| {
                ws.notes_doc = Some((id, doc));
                cx.notify();
            });
        })
        .detach();
    }

    fn show(&mut self, screen: Screen, window: &mut Window, cx: &mut Context<Self>) {
        let search = screen == Screen::Search;
        if screen == Screen::ActionItems {
            self.load_all_actions(cx);
        }
        self.screen = screen;
        if search {
            self.search_input.update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }

    pub(crate) fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }

    /// Jump to a moment of the selected meeting: open its transcript there.
    pub(crate) fn cue(&mut self, at: i64, cx: &mut Context<Self>) {
        if self.selected().is_none() {
            return;
        }
        self.playhead = at;
        self.tab = Tab::Transcript;
        if let Some(pb) = &mut self.playback {
            pb.seek_ms(at.max(0) as u64 * 1000);
        }
        cx.notify();
    }

    /// Read the selected meeting's transcript again (it was just written).
    pub(crate) fn reload_transcript(&mut self, cx: &mut Context<Self>) {
        self.transcript = None;
        self.load_transcript(cx);
    }

    /// Read the selected meeting's transcript on a background thread.
    fn load_transcript(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected().map(|m| m.id.clone()) else {
            return;
        };
        if self.transcript.as_ref().is_some_and(|(t, _)| *t == id) {
            return;
        }
        let store = self.store.clone();
        let task = cx.background_executor().spawn({
            let id = id.clone();
            async move {
                match store.lock() {
                    Ok(s) => s.segments(&id),
                    Err(_) => Ok(Vec::new()),
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let segments = task.await;
            let _ = this.update(cx, |ws, cx| {
                match segments {
                    Ok(segs) => ws.transcript = Some((id, segs)),
                    Err(e) => tracing::warn!(error = %e, "could not read the transcript"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Answer an Ask panel question with the chosen CLI and the MCP server.
    fn answer(&mut self, question: String, cx: &mut Context<Self>) {
        let scope = match (self.ask.read(cx).scope(), self.selected()) {
            (Scope::Meeting, Some(m)) => {
                switchscribe_agent::ask::AskScope::Meeting(m.id.clone(), m.title.clone())
            }
            _ => switchscribe_agent::ask::AskScope::All,
        };
        let cli = match &self.agent.cli {
            Some(Ok(program)) => Ok(switchscribe_agent::Cli {
                kind: self.settings.agent.cli,
                program: program.clone(),
            }),
            Some(Err(e)) => Err(e.clone()),
            None => Err(format!(
                "Still looking for {}.",
                self.settings.agent.cli.label()
            )),
        };
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("switchscribe"));
        let mut args = vec!["mcp".to_owned(), "--ask".to_owned()];
        if self.demo {
            // The demo's meetings live in memory; the server rebuilds the same data.
            args.push("--demo".to_owned());
        }
        let req = AskRequest {
            question,
            scope,
            cli,
            mcp: switchscribe_agent::McpServer { command: exe, args },
            me: self.people.iter().find(|p| p.is_me).map(|p| p.name.clone()),
            workdir: if self.demo {
                std::env::temp_dir().join("switchscribe-demo").join("agent")
            } else {
                self.paths.data.join("agent")
            },
        };
        self.ask.update(cx, |a, cx| a.run(req, cx));
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().to_string();
        self.search_generation += 1;
        let generation = self.search_generation;
        let store = self.store.clone();
        let filter = SearchFilter {
            person: self.search_person,
            since: self
                .search_days
                .map(|d| Local::now().timestamp() - i64::from(d) * 86_400),
            meeting: None,
        };
        let task = cx.background_executor().spawn(async move {
            match store.lock() {
                Ok(s) => s.search_with(&query, &filter, 200),
                Err(_) => Ok(Vec::new()),
            }
        });
        cx.spawn(async move |this, cx| {
            let hits = task.await;
            let _ = this.update(cx, |ws, cx| {
                // A newer query may have finished first.
                if ws.search_generation != generation {
                    return;
                }
                match hits {
                    Ok(h) => ws.search_hits = h,
                    Err(e) => tracing::warn!(error = %e, "search failed"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Write the settings file on a background thread (never for the demo, which leaves
    /// the data folder alone).
    /// Keep audio: Forever, 7, 30, 90 days.
    fn cycle_keep_audio(&mut self, cx: &mut Context<Self>) {
        const STEPS: [Option<u32>; 4] = [None, Some(7), Some(30), Some(90)];
        let now = STEPS
            .iter()
            .position(|d| *d == self.settings.storage.keep_audio_days)
            .unwrap_or(0);
        self.settings.storage.keep_audio_days = STEPS[(now + 1) % STEPS.len()];
        self.save_settings(cx);
        cx.notify();
    }

    /// Delete old audio per Settings, off the UI thread (at start).
    fn prune_audio(&self, cx: &mut Context<Self>) {
        let Some(days) = self.settings.storage.keep_audio_days else {
            return;
        };
        if self.demo {
            return;
        }
        let paths = self.paths.clone();
        let meetings = self.meetings.clone();
        cx.background_executor()
            .spawn(async move {
                let n = paths.prune_audio(&meetings, days, chrono::Utc::now().timestamp());
                if n > 0 {
                    tracing::info!(meetings = n, days, "deleted old meeting audio");
                }
            })
            .detach();
    }

    pub(crate) fn save_settings(&self, cx: &mut Context<Self>) {
        if self.demo {
            return;
        }
        let settings = self.settings.clone();
        let path = self.paths.settings_file();
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = settings.save(&path) {
                    tracing::warn!(error = %e, "could not save settings");
                }
            })
            .detach();
    }

    fn toggle_ask(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_open = !self.ask_open;
        self.settings.layout.ask_open = Some(self.ask_open);
        if self.ask_open {
            self.ask.update(cx, |a, cx| a.focus_input(window, cx));
        }
        self.save_settings(cx);
        cx.notify();
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.settings.layout.sidebar_open = !self.settings.layout.sidebar_open;
        self.save_settings(cx);
        cx.notify();
    }

    pub(crate) fn cycle_cli(&mut self, cx: &mut Context<Self>) {
        let cli = self.settings.agent.cli.next();
        self.settings.agent.cli = cli;
        self.ask.update(cx, |a, cx| a.set_cli(cli, cx));
        self.detect_cli(cx);
        self.save_settings(cx);
        cx.notify();
    }

    /// Start recording, or stop the recording in progress.
    pub(crate) fn toggle_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.recording.is_some() {
            self.stop_recording(cx);
        } else {
            self.start_recording(false, window, cx);
        }
    }

    pub(crate) fn backend(&self) -> Result<Backend, String> {
        if self.demo || recording::fake_audio_requested() {
            return Ok(Backend::fake());
        }
        let mode = match self.settings.audio.capture_mode {
            CaptureMode::TeamsOnly => RemoteMode::TeamsOnly,
            CaptureMode::WholeSystem => RemoteMode::WholeSystem,
        };
        Backend::system(mode, self.settings.audio.mic_device.clone()).map_err(|e| e.to_string())
    }

    /// Create the meeting and its folder and start both tracks, off the UI thread.
    fn start_recording(&mut self, test: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.start_recording_as(None, test, window, cx);
    }

    /// Record a detected meeting under its name.
    pub(crate) fn start_titled_recording(
        &mut self,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_recording_as(Some(title), false, window, cx);
    }

    fn start_recording_as(
        &mut self,
        title: Option<String>,
        test: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.starting || self.recording.is_some() {
            return;
        }
        self.ignore_offer(cx);
        self.banners.clear();
        let backend = match self.backend() {
            Ok(b) => b,
            Err(e) => {
                self.banners.push(Banner::plain(format!(
                    "Cannot record here: {e}. Set SWITCHSCRIBE_FAKE_AUDIO=1 to try the recorder with synthetic voices."
                )));
                cx.notify();
                return;
            }
        };
        let now = Local::now();
        let id = recording::meeting_id(now);
        let event = (!test).then(|| self.current_event()).flatten();
        let title = if test {
            "Test recording".to_owned()
        } else {
            title
                .or_else(|| event.as_ref().map(|e| e.subject.clone()))
                .unwrap_or_else(|| recording::default_title(now))
        };
        let dir = self.meeting_dir(&id);
        let me = self.people.iter().find(|p| p.is_me).cloned();
        let row = NewMeeting {
            id: id.clone(),
            title: title.clone(),
            started_at: now.timestamp(),
            duration_secs: 0,
            source_app: "Teams".into(),
            status: MeetingStatus::Recording,
            attendees: me.iter().map(|p| p.id).collect(),
        };
        self.starting = true;
        cx.notify();
        let store = self.store.clone();
        let task = cx.background_executor().spawn({
            let dir = dir.clone();
            async move {
                let recorder = Recorder::start(&dir, backend).map_err(|e| e.to_string())?;
                match store.lock() {
                    Ok(mut s) => {
                        if let Err(e) = s.insert_meeting(&row) {
                            tracing::warn!(error = %e, "could not add the meeting to the index");
                        }
                        if let Some(event) = &event {
                            crate::integrations_view::attach_event(&s, &row.id, event);
                        }
                    }
                    Err(_) => tracing::warn!("the index lock is poisoned"),
                }
                Ok::<_, String>(recorder)
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let started = task.await;
            let _ = this.update_in(cx, |ws, window, cx| {
                ws.starting = false;
                match started {
                    Ok(recorder) => {
                        ws.begin_session(id, title, dir, recorder, me, now, test, window, cx)
                    }
                    Err(e) => {
                        ws.banners
                            .push(Banner::plain(format!("Could not start recording: {e}")));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_session(
        &mut self,
        id: String,
        title: String,
        dir: PathBuf,
        recorder: Recorder,
        me: Option<Person>,
        now: DateTime<Local>,
        test: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let monitor = recorder.monitor();
        let captions = match self.start_captions(&recorder) {
            Ok(c) => c,
            Err(banner) => {
                self.banners.push(banner);
                None
            }
        };
        let notes =
            cx.new(|cx| TextareaState::new(window, cx).placeholder("Notes stay on this machine."));
        let notes_changed = cx.subscribe(&notes, |this, _, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                this.notes_changed(cx);
            }
        });
        // Meters, the clock and notices refresh ten times a second while recording.
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if this.update(cx, |ws, cx| ws.tick(cx)).is_err() {
                    break;
                }
            }
        });
        self.meetings.insert(
            0,
            Meeting {
                id: id.clone(),
                title: title.clone(),
                started_at: now.timestamp(),
                duration_secs: 0,
                source_app: "Teams".into(),
                status: MeetingStatus::Recording,
                attendees: me.into_iter().collect(),
            },
        );
        self.recording = Some(Session {
            meeting_id: id,
            title,
            dir,
            recorder: Some(recorder),
            monitor,
            notes,
            bookmarks: Vec::new(),
            notes_generation: 0,
            test,
            quiet_since: std::time::Instant::now(),
            captions,
            _tick: tick,
            _notes_changed: notes_changed,
        });
        self.screen = Screen::Live;
        // Opened after this update: the new window renders at once and reads the session.
        let ws = cx.weak_entity();
        cx.defer(move |cx| {
            let Some(handle) = mini_recorder::open(ws.clone(), cx) else {
                return;
            };
            let kept = ws.update(cx, |ws, _| {
                let live = ws.recording.is_some();
                if live {
                    ws.mini = Some(handle);
                }
                live
            });
            if !kept.unwrap_or(false) {
                mini_recorder::close(handle, cx);
            }
        });
        self.set_tray(RecorderState::Recording);
        cx.notify();
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let Some(s) = &self.recording else {
            return;
        };
        for notice in s.monitor.take_notices() {
            let banner = recording::banner_for(&notice);
            if !self.banners.contains(&banner) {
                self.banners.push(banner);
            }
        }
        let (id, secs, paused) = (s.meeting_id.clone(), s.elapsed_secs(), s.paused());
        self.poll_captions(cx);
        if let Some(m) = self.meetings.iter_mut().find(|m| m.id == id) {
            m.duration_secs = secs as u32;
        }
        self.set_tray(if paused {
            RecorderState::Paused
        } else {
            RecorderState::Recording
        });
        if let Some(mini) = self.mini {
            let _ = mini.update(cx, |_, window, _| window.refresh());
        }
        self.check_auto_stop(cx);
        cx.notify();
    }

    /// Pause or resume the recording.
    pub(crate) fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = &self.recording
            && let Some(r) = &s.recorder
        {
            r.set_paused(!s.paused());
        }
        self.tick(cx);
    }

    /// Mark the current moment.
    pub(crate) fn bookmark(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = &mut self.recording
            && let Some(r) = &mut s.recorder
        {
            let at = r.bookmark();
            s.bookmarks.push(at);
            cx.notify();
        }
    }

    /// Bring the main window forward on the live view and start a timestamped line in
    /// the notes.
    pub(crate) fn quick_note(&mut self, cx: &mut Context<Self>) {
        let Some(s) = &self.recording else {
            return;
        };
        let line = recording::anchor_line(s.elapsed_secs());
        let notes = s.notes.clone();
        self.screen = Screen::Live;
        cx.notify();
        let main = self.window;
        let ws = cx.weak_entity();
        // Deferred: this may run inside the mini recorder's click handler.
        cx.defer(move |cx| {
            let _ = main.update(cx, |_, window, cx| {
                window.activate_window();
                notes.update(cx, |state, cx| {
                    let mut text = state.value().to_string();
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&line);
                    let row = text.matches('\n').count() as u32;
                    let col =
                        text.rsplit('\n')
                            .next()
                            .map_or(0, |l| l.encode_utf16().count()) as u32;
                    state.set_value(text, window, cx);
                    state.set_cursor_position(Position::new(row, col), window, cx);
                });
            });
            let _ = ws.update(cx, |ws, cx| ws.notes_changed(cx));
        });
    }

    /// Save the live notes shortly after typing stops.
    fn notes_changed(&mut self, cx: &mut Context<Self>) {
        let Some(s) = &mut self.recording else {
            return;
        };
        s.notes_generation += 1;
        let generation = s.notes_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(600))
                .await;
            let _ = this.update(cx, |ws, cx| {
                let Some(s) = &ws.recording else { return };
                if s.notes_generation != generation {
                    return;
                }
                let text = s.notes.read(cx).value().to_string();
                let dir = s.dir.clone();
                cx.background_executor()
                    .spawn(async move {
                        if let Err(e) = recording::save_notes(&dir, &text) {
                            tracing::warn!(error = %e, "could not save notes");
                        }
                    })
                    .detach();
            });
        })
        .detach();
    }

    /// Stop recording: finish both tracks, save the notes and mark the meeting for
    /// processing, off the UI thread.
    pub(crate) fn stop_recording(&mut self, cx: &mut Context<Self>) {
        let Some(mut s) = self.recording.take() else {
            return;
        };
        let Some(recorder) = s.recorder.take() else {
            return;
        };
        let notes = s.notes.read(cx).value().to_string();
        let id = s.meeting_id.clone();
        let dir = s.dir.clone();
        drop(s);
        if let Some(mini) = self.mini.take() {
            // Deferred: the click may have come from the mini recorder itself.
            cx.defer(move |cx| mini_recorder::close(mini, cx));
        }
        self.set_tray(RecorderState::Processing);
        if self.screen == Screen::Live {
            self.screen = Screen::Meeting(id.clone());
            self.tab = Tab::Notes;
        }
        cx.notify();
        let store = self.store.clone();
        let task = cx.background_executor().spawn({
            let id = id.clone();
            async move {
                let rec = recorder.stop().map_err(|e| e.to_string())?;
                if !notes.trim().is_empty()
                    && let Err(e) = recording::save_notes(&dir, &notes)
                {
                    tracing::warn!(error = %e, "could not save notes");
                }
                if let Ok(s) = store.lock() {
                    let secs = (rec.duration_ms / 1000) as u32;
                    let saved = s
                        .set_duration(&id, secs)
                        .and_then(|()| s.set_status(&id, MeetingStatus::Processing));
                    if let Err(e) = saved {
                        tracing::warn!(error = %e, "could not update the meeting");
                    }
                }
                Ok::<_, String>(rec)
            }
        });
        cx.spawn(async move |this, cx| {
            let finished = task.await;
            let _ = this.update(cx, |ws, cx| {
                match finished {
                    Ok(rec) => {
                        if let Some(m) = ws.meetings.iter_mut().find(|m| m.id == id) {
                            m.duration_secs = (rec.duration_ms / 1000) as u32;
                            m.status = MeetingStatus::Processing;
                        }
                        ws.banners.extend(recording::empty_recording(&rec));
                        if rec.tracks.iter().any(|t| t.file.is_some()) {
                            ws.queue_transcription(id.clone(), cx);
                        }
                    }
                    Err(e) => ws.banners.push(Banner::plain(format!(
                        "Could not finish the recording: {e}"
                    ))),
                }
                if ws.jobs.transcription(&id).is_none() {
                    ws.set_tray(RecorderState::Idle);
                }
                ws.notes_doc = None;
                ws.load_notes(cx);
                ws.load_playback(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// The app is quitting: finish the recording in place (blocking, briefly) so both
    /// files end cleanly.
    fn finish_on_quit(&mut self, cx: &mut Context<Self>) {
        let Some(mut s) = self.recording.take() else {
            return;
        };
        let Some(recorder) = s.recorder.take() else {
            return;
        };
        let notes = s.notes.read(cx).value().to_string();
        match recorder.stop() {
            Ok(rec) => {
                if let Ok(store) = self.store.lock() {
                    let _ = store.set_duration(&s.meeting_id, (rec.duration_ms / 1000) as u32);
                    let _ = store.set_status(&s.meeting_id, MeetingStatus::Processing);
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not finish the recording on quit"),
        }
        if !notes.trim().is_empty() {
            let _ = recording::save_notes(&s.dir, &notes);
        }
    }

    pub(crate) fn set_tray(&mut self, state: RecorderState) {
        #[cfg(windows)]
        if let Some(tray) = &mut self.tray {
            tray.set_state(state);
        }
        #[cfg(not(windows))]
        let _ = state;
    }

    /// Act on tray menu picks.
    #[cfg(windows)]
    fn poll_tray(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::tray::TrayCommand;
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let alive = this.update_in(cx, |ws, window, cx| {
                    let commands = ws.tray.as_ref().map(|t| t.poll()).unwrap_or_default();
                    for command in commands {
                        match command {
                            TrayCommand::ToggleRecording => ws.toggle_recording(window, cx),
                            TrayCommand::Show => window.activate_window(),
                            TrayCommand::Quit => cx.quit(),
                        }
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn set_capture_mode(&mut self, mode: CaptureMode, cx: &mut Context<Self>) {
        self.settings.audio.capture_mode = mode;
        self.save_settings(cx);
        cx.notify();
    }

    fn set_mic(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.settings.audio.mic_device = id;
        self.save_settings(cx);
        cx.notify();
    }

    /// List the microphones again, off the UI thread.
    fn refresh_devices(&mut self, cx: &mut Context<Self>) {
        let task = cx
            .background_executor()
            .spawn(async move { switchscribe_capture::input_devices() });
        cx.spawn(async move |this, cx| {
            let devices = task.await;
            let _ = this.update(cx, |ws, cx| {
                ws.devices = devices;
                cx.notify();
            });
        })
        .detach();
    }

    fn render_title_bar(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let sidebar_open = self.settings.layout.sidebar_open;
        let brand = div()
            .flex_none()
            .w(rpx(236.))
            .flex()
            .items_center()
            .gap(rpx(8.))
            .child(
                div()
                    .id("toggle-sidebar")
                    .size(rpx(14.))
                    .rounded(px(3.))
                    .bg(if sidebar_open { p.fg } else { p.fg3 })
                    .occlude()
                    .cursor_pointer()
                    .tooltip(|w, cx| {
                        Tooltip::new("Toggle sidebar")
                            .action(&actions::ToggleSidebar, None)
                            .build(w, cx)
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
            )
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(ts::BASE)
                    .child("switchscribe"),
            );
        let search = div()
            .id("title-search")
            .flex_1()
            .max_w(rpx(520.))
            .h(rpx(28.))
            .flex()
            .items_center()
            .gap(rpx(8.))
            .px(rpx(10.))
            .border_1()
            .border_color(p.bd2)
            .rounded(px(6.))
            .bg(p.bg)
            .text_color(p.fg3)
            .text_size(ts::BASE)
            .occlude()
            .cursor_text()
            .on_click(cx.listener(|this, _, w, cx| this.show(Screen::Search, w, cx)))
            .child(div().flex_1().truncate().child("Search all meetings"))
            .child(ui::kbd("Ctrl+Shift+F", p));
        let record = match &self.recording {
            // While recording: a red pill with the elapsed time that opens the live view.
            Some(s) => ui::button("record", "", Kind::Secondary, p)
                .occlude()
                .px(rpx(12.))
                .border_color(p.rec)
                .bg(p.rec.opacity(0.16))
                .text_color(p.rec_fg)
                .font_weight(FontWeight::MEDIUM)
                .tooltip(|w, cx| {
                    Tooltip::new("Show the live view")
                        .action(&actions::ToggleRecording, None)
                        .build(w, cx)
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.screen = Screen::Live;
                    cx.notify();
                }))
                .child(ui::dot(8., p.rec).opacity(s.pulse()))
                .child(ui::mono(
                    crate::format::clock(s.elapsed_secs()),
                    ts::BODY,
                    p.rec_fg,
                )),
            None => ui::button("record", "", Kind::Secondary, p)
                .occlude()
                .px(rpx(12.))
                .bg(p.elev)
                .text_color(p.fg)
                .font_weight(FontWeight::MEDIUM)
                .when(self.starting, |d| d.opacity(0.6))
                .tooltip(|w, cx| {
                    Tooltip::new("Record Teams and your mic")
                        .action(&actions::ToggleRecording, None)
                        .build(w, cx)
                })
                .on_click(cx.listener(|this, _, w, cx| this.start_recording(false, w, cx)))
                .child(
                    div()
                        .size(rpx(8.))
                        .rounded_full()
                        .border(px(1.5))
                        .border_color(p.fg2),
                )
                .child(if self.starting {
                    "Starting…"
                } else {
                    "Record"
                })
                .child(ui::kbd("Ctrl+Shift+R", p)),
        };
        // Amber when the CLI is missing: summaries and Ask cannot run.
        let (cli_dot, cli_tip) = match &self.agent.cli {
            Some(Ok(_)) => (p.ok, "Coding CLI for summaries and Ask".to_owned()),
            Some(Err(e)) => (p.warn, format!("{e} Click to try another CLI.")),
            None => (p.fg4, "Looking for the CLI…".to_owned()),
        };
        let cli = ui::button("cli", "", Kind::Outline, p)
            .occlude()
            .gap(rpx(6.))
            .tooltip(move |w, cx| Tooltip::new(cli_tip.clone()).build(w, cx))
            .on_click(cx.listener(|this, _, _, cx| this.cycle_cli(cx)))
            .child(ui::dot(6., cli_dot))
            .child(self.settings.agent.cli.label())
            .child(div().text_color(p.fg4).child("▾"));
        let ask = ui::button("toggle-ask", "Ask", Kind::Outline, p)
            .occlude()
            .when(self.ask_open, |d| d.bg(p.sel))
            .on_click(cx.listener(|this, _, w, cx| this.toggle_ask(w, cx)))
            .child(ui::kbd("Ctrl+J", p));
        let gear = div()
            .id("settings")
            .size(rpx(28.))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .border_1()
            .border_color(p.bd2)
            .rounded(px(6.))
            .occlude()
            .cursor_pointer()
            .when(self.screen == Screen::Settings, |d| d.bg(p.sel))
            .hover(|s| s.bg(p.elev))
            .tooltip(|w, cx| {
                Tooltip::new("Settings")
                    .action(&actions::ShowSettings, None)
                    .build(w, cx)
            })
            .on_click(cx.listener(|this, _, w, cx| this.show(Screen::Settings, w, cx)))
            .child(
                svg()
                    .path("icons/settings.svg")
                    .size(rpx(14.))
                    .text_color(p.fg2),
            );
        TitleBar::new()
            .h(rpx(44.))
            .bg(p.panel)
            .border_color(p.bd)
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap(rpx(12.))
                    .px(rpx(12.))
                    .font_family(SANS)
                    .text_color(p.fg)
                    .child(brand)
                    .child(search)
                    .child(div().flex_1())
                    .child(record)
                    .children(crate::updates::title_button(self, p, cx))
                    .child(cli)
                    .child(ask)
                    .child(gear),
            )
            .into_any_element()
    }

    fn render_sidebar(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let groups = group_by_date(&self.meetings, self.today);
        let selected = match &self.screen {
            Screen::Meeting(id) => Some(id.as_str()),
            Screen::Live => self.recording.as_ref().map(|s| s.meeting_id.as_str()),
            _ => None,
        };
        let list = div()
            .id("meetings")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(rpx(6.))
            .pb(rpx(8.))
            .when(groups.is_empty(), |d| {
                d.child(
                    div()
                        .px(rpx(8.))
                        .py(rpx(10.))
                        .text_size(ts::BODY)
                        .text_color(p.fg3)
                        .line_height(rpx(18.))
                        .child("Meetings you record show up here."),
                )
            })
            .children(groups.into_iter().map(|(group, items)| {
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(rpx(8.))
                            .pt(rpx(10.))
                            .pb(rpx(4.))
                            .text_size(ts::SMALL)
                            .text_color(p.fg4)
                            .child(group.label()),
                    )
                    .children(items.into_iter().map(|m| {
                        let on = selected == Some(m.id.as_str());
                        self.render_meeting_row(m, on, p, cx)
                    }))
            }));
        let nav = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(1.))
            .p(rpx(6.))
            .border_t_1()
            .border_color(p.bd)
            .children(
                [
                    (Screen::ActionItems, "Action items", {
                        let open = self
                            .agent
                            .all_items
                            .iter()
                            .filter(|i| i.status != ActionStatus::Done)
                            .count();
                        if open == 0 {
                            String::new()
                        } else {
                            open.to_string()
                        }
                    }),
                    (Screen::People, "People", self.people.len().to_string()),
                    (Screen::Search, "Search", "Ctrl+Shift+F".to_owned()),
                ]
                .map(|(screen, label, meta)| {
                    let on = self.screen == screen;
                    div()
                        .id(label)
                        .h(rpx(30.))
                        .flex()
                        .items_center()
                        .gap(rpx(8.))
                        .px(rpx(8.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_color(if on { p.fg } else { p.fg_nav })
                        .when(on, |d| d.bg(p.sel))
                        .hover(|s| s.bg(p.elev))
                        .on_click(
                            cx.listener(move |this, _, w, cx| this.show(screen.clone(), w, cx)),
                        )
                        .child(div().flex_1().child(label))
                        .child(ui::mono(meta, ts::SMALL, p.fg4))
                }),
            );
        div()
            .w(rpx(260.))
            .flex_none()
            .flex()
            .flex_col()
            .min_h_0()
            .border_r_1()
            .border_color(p.bd)
            .bg(p.panel)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(rpx(12.))
                    .pt(rpx(12.))
                    .pb(rpx(6.))
                    .child(ui::section_label("Meetings", p.fg3))
                    .child(ui::mono(self.meetings.len().to_string(), ts::SMALL, p.fg4)),
            )
            .child(list)
            .child(nav)
            .into_any_element()
    }

    fn render_meeting_row(
        &self,
        m: &Meeting,
        on: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = m.id.clone();
        let status: Option<AnyElement> = match m.status {
            MeetingStatus::Recording => Some(ui::dot(7., p.rec).into_any_element()),
            MeetingStatus::Processing => Some(
                svg()
                    .path("icons/loader-circle.svg")
                    .size(rpx(11.))
                    .text_color(p.fg2)
                    .into_any_element(),
            ),
            MeetingStatus::NeedsLabels => Some(
                div()
                    .px(rpx(4.))
                    .rounded(px(3.))
                    .border_1()
                    .border_dashed()
                    .border_color(p.fg3)
                    .font_family(MONO)
                    .text_size(ts::TINY)
                    .text_color(p.fg_ctl)
                    .child("?")
                    .into_any_element(),
            ),
            MeetingStatus::Ready => None,
        };
        div()
            .id(SharedString::from(format!("meeting-{}", m.id)))
            .w_full()
            .p(rpx(8.))
            .rounded(px(6.))
            .cursor_pointer()
            .flex()
            .flex_col()
            .gap(rpx(5.))
            .when(on, |d| d.bg(p.sel))
            .when(!on, |d| d.hover(|s| s.bg(p.elev)))
            .on_click(cx.listener(move |this, _, _, cx| this.select(id.clone(), cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(6.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(m.title.clone()),
                    )
                    .children(status),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(ui::mono(
                        crate::format::row_time(m, self.today),
                        ts::SMALL,
                        p.fg3,
                    ))
                    .child(ui::mono(
                        crate::format::duration_short(m.duration_secs),
                        ts::SMALL,
                        p.fg3,
                    ))
                    .child(div().flex_1())
                    .child(
                        // Overlapping avatars, 4 px apart like the prototype.
                        div()
                            .relative()
                            .flex_none()
                            .h(rpx(16.))
                            .w(rpx(4. + 12. * m.attendees.len() as f32))
                            .children(m.attendees.iter().enumerate().map(|(i, a)| {
                                ui::avatar(a.initials(), p.speaker(a.color), 16., false, p)
                                    .absolute()
                                    .top_0()
                                    .left(rpx(12. * i as f32))
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_main(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let content: AnyElement = match &self.screen {
            Screen::Meeting(_) => match self.selected() {
                Some(m) => meeting_view::render(self, m, p, cx),
                None => self.render_empty(p, cx),
            },
            Screen::Live => match &self.recording {
                Some(s) => live_view::render(self, s, p, cx),
                None => self.render_empty(p, cx),
            },
            Screen::Empty => self.render_empty(p, cx),
            Screen::ActionItems => agent_view::action_items_screen(self, p, cx),
            Screen::People => speakers::people_screen(self, p, cx),
            Screen::Search => self.render_search(p, cx),
            Screen::Settings => self.render_settings(p, cx),
        };
        let mut banners: Vec<Banner> = Vec::new();
        if let Some(e) = &self.store_error {
            banners.push(Banner::plain(format!(
                "Could not open the meetings database ({e}). Nothing is being saved."
            )));
        }
        banners.extend(self.banners.iter().cloned());
        let own = self.banners.len();
        let offset = banners.len() - own;
        let notice = crate::integrations_view::notice_strip(self, p, cx);
        div()
            .flex_1()
            .min_w(rpx(480.))
            .flex()
            .flex_col()
            .bg(p.bg)
            .children(banners.into_iter().enumerate().map(|(i, b)| {
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(rpx(10.))
                    .px(rpx(16.))
                    .py(rpx(8.))
                    .bg(p.warn.opacity(0.1))
                    .border_b_1()
                    .border_color(p.warn.opacity(0.35))
                    .text_color(p.warn_fg)
                    .child(
                        div()
                            .size(rpx(16.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.))
                            .bg(p.warn)
                            .text_color(p.on_light)
                            .text_size(ts::SMALL)
                            .font_weight(FontWeight::BOLD)
                            .child("!"),
                    )
                    .child(div().flex_1().min_w_0().child(b.text))
                    .when_some(b.action.clone(), |d, (label, screen)| {
                        d.child(
                            div()
                                .id(("banner-action", i))
                                .text_size(ts::BODY)
                                .underline()
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.show(screen.clone(), w, cx)
                                }))
                                .child(label),
                        )
                    })
                    .when(i >= offset, |d| {
                        let k = i - offset;
                        d.child(
                            div()
                                .id(("banner-x", i))
                                .text_color(p.warn_fg)
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if k < this.banners.len() {
                                        this.banners.remove(k);
                                    }
                                    cx.notify();
                                }))
                                .child("✕"),
                        )
                    })
            }))
            .children(notice)
            .child(content)
            .into_any_element()
    }

    fn render_empty(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        ui::empty_state(
            "No meetings yet",
            "Record a Teams meeting and switchscribe keeps the audio, transcript and notes on this machine.",
            p,
        )
        .child(
            div()
                .mt(rpx(8.))
                .flex()
                .items_center()
                .gap(rpx(12.))
                .child(
                    ui::button("empty-record", "Record", Kind::Primary, p)
                        .px(rpx(12.))
                        .on_click(
                            cx.listener(|this, _, w, cx| this.start_recording(false, w, cx)),
                        ),
                )
                .child(
                    div()
                        .id("test-recording")
                        .text_size(ts::BODY)
                        .text_color(p.stamp)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, w, cx| this.start_recording(true, w, cx)))
                        .child("Start a test recording"),
                ),
        )
        .into_any_element()
    }

    fn render_search(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let query = self.search_input.read(cx).value().trim().to_string();
        // Group hits by meeting, in the order their best hit ranks.
        let mut groups: Vec<(String, String, Vec<&SearchHit>)> = Vec::new();
        for h in &self.search_hits {
            match groups.iter_mut().find(|g| g.0 == h.meeting_id) {
                Some(g) => g.2.push(h),
                None => groups.push((h.meeting_id.clone(), h.meeting_title.clone(), vec![h])),
            }
        }
        let results = div()
            .id("search-results")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(rpx(20.))
            .pt(rpx(12.))
            .pb(rpx(20.))
            .flex()
            .flex_col()
            .gap(rpx(18.))
            .when(!query.is_empty() && groups.is_empty(), |d| {
                d.child(
                    div()
                        .px(rpx(8.))
                        .py(rpx(20.))
                        .text_color(p.fg3)
                        .child(format!(
                            "Nothing found across {} meetings.",
                            self.meetings.len()
                        )),
                )
            })
            .children(
                groups
                    .into_iter()
                    .enumerate()
                    .map(|(gi, (id, title, hits))| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(rpx(4.))
                            .child(
                                div()
                                    .px(rpx(8.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .children(hits.into_iter().enumerate().map(|(hi, h)| {
                                let (name, color) = match h.person_id.and_then(|i| self.person(i)) {
                                    Some(person) => (person.name.clone(), p.speaker(person.color)),
                                    None => (h.speaker_label.clone(), p.unknown),
                                };
                                let id = id.clone();
                                let at = h.start_ms / 1000;
                                div()
                                    .id(("hit", gi * 1000 + hi))
                                    .flex()
                                    .items_start()
                                    .gap(rpx(12.))
                                    .p(rpx(8.))
                                    .rounded(px(6.))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(p.surface))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.select(id.clone(), cx);
                                        this.cue(at, cx);
                                    }))
                                    .child(
                                        div()
                                            .w(rpx(120.))
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .gap(rpx(6.))
                                            .text_size(ts::BODY)
                                            .text_color(p.fg_chip)
                                            .child(ui::dot(7., color))
                                            .child(div().truncate().child(name)),
                                    )
                                    .child(div().w(rpx(60.)).flex_none().child(ui::mono(
                                        crate::format::clock(at),
                                        ts::SMALL,
                                        p.stamp,
                                    )))
                                    .child(snippet(&h.snippet, p))
                            }))
                    }),
            );
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(rpx(10.))
                    .px(rpx(20.))
                    .pt(rpx(16.))
                    .pb(rpx(10.))
                    .border_b_1()
                    .border_color(p.bd)
                    .child(Input::new(&self.search_input).text_size(ts::LARGE))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(rpx(6.))
                            .text_size(ts::BODY)
                            .child(div().text_color(p.fg3).child("Person"))
                            .children(self.people.iter().map(|person| {
                                let on = self.search_person == Some(person.id);
                                let id = person.id;
                                let color = p.speaker(person.color);
                                div()
                                    .id(("search-person", id as usize))
                                    .flex()
                                    .items_center()
                                    .gap(rpx(5.))
                                    .px(rpx(8.))
                                    .py(rpx(3.))
                                    .rounded(px(12.))
                                    .border_1()
                                    .border_color(if on { color } else { p.bd2 })
                                    .text_color(if on { p.fg } else { p.fg_ctl })
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.search_person = if on { None } else { Some(id) };
                                        this.run_search(cx);
                                    }))
                                    .child(ui::dot(7., color))
                                    .child(person.name.clone())
                            }))
                            .child(div().w(rpx(10.)))
                            .child(div().text_color(p.fg3).child("Date"))
                            .children(
                                [
                                    (None, "Any time"),
                                    (Some(7), "Past week"),
                                    (Some(30), "Past month"),
                                ]
                                .into_iter()
                                .map(|(days, label)| {
                                    let on = self.search_days == days;
                                    div()
                                        .id(label)
                                        .px(rpx(8.))
                                        .py(rpx(3.))
                                        .rounded(px(12.))
                                        .border_1()
                                        .border_color(if on { p.fg2 } else { p.bd2 })
                                        .text_color(if on { p.fg } else { p.fg_ctl })
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.search_days = days;
                                            this.run_search(cx);
                                        }))
                                        .child(label)
                                }),
                            )
                            .child(div().flex_1())
                            .child(ui::mono(
                                match self.search_hits.len() {
                                    1 => "1 hit".to_owned(),
                                    n => format!("{n} hits"),
                                },
                                ts::BODY,
                                p.fg3,
                            )),
                    ),
            )
            .child(results)
            .into_any_element()
    }

    /// The capture-mode toggle, its explanation and the microphone list (Settings and
    /// onboarding).
    pub(crate) fn audio_controls(
        &self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> (AnyElement, &'static str, AnyElement) {
        let s = &self.settings;
        // Audio: the capture mode as a two-way toggle, the microphone as a radio list.
        let mode = s.audio.capture_mode;
        let modes = div()
            .flex()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(6.))
            .bg(p.chip)
            .children(
                [
                    (CaptureMode::TeamsOnly, "Teams only"),
                    (CaptureMode::WholeSystem, "Whole system"),
                ]
                .map(|(m, label)| {
                    let active = m == mode;
                    div()
                        .id(label)
                        .px(rpx(10.))
                        .py(rpx(4.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .text_size(ts::BODY)
                        .text_color(if active { p.fg } else { p.fg2 })
                        .when(active, |d| d.bg(p.raised))
                        .on_click(cx.listener(move |this, _, _, cx| this.set_capture_mode(m, cx)))
                        .child(label)
                }),
            );
        let mode_note = match mode {
            CaptureMode::TeamsOnly => {
                "Records Teams' own audio and nothing else; falls back to the whole system if Teams is not running."
            }
            CaptureMode::WholeSystem => "Records everything your speakers or headset play.",
        };
        let chosen = s.audio.mic_device.clone();
        let mic_option = |id: Option<String>, name: String, i: usize| {
            let active = chosen == id;
            div()
                .id(("mic", i))
                .flex()
                .items_center()
                .gap(rpx(8.))
                .py(rpx(3.))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.set_mic(id.clone(), cx)))
                .child(
                    div()
                        .size(rpx(12.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .border_1()
                        .border_color(if active { p.fg } else { p.fg4 })
                        .when(active, |d| d.child(ui::dot(6., p.fg))),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(if active { p.fg } else { p.fg_ctl })
                        .child(name),
                )
        };
        let missing = chosen
            .as_ref()
            .filter(|id| !self.devices.iter().any(|d| &d.id == *id));
        let mics = div()
            .flex()
            .flex_col()
            .child(mic_option(None, "Default communications device".into(), 0))
            .children(
                self.devices
                    .iter()
                    .enumerate()
                    .map(|(i, d)| mic_option(Some(d.id.clone()), d.name.clone(), i + 1)),
            )
            .when_some(missing, |d, _| {
                d.child(
                    div()
                        .text_size(ts::BODY)
                        .text_color(p.warn_fg)
                        .child("The saved microphone is not connected; the default is used."),
                )
            })
            .when(self.devices.is_empty(), |d| {
                d.child(div().text_size(ts::BODY).text_color(p.fg3).child(
                    if cfg!(any(windows, target_os = "macos")) {
                        "No microphones found."
                    } else {
                        "No microphones found. Capture needs Windows or macOS."
                    },
                ))
            })
            .child(
                div()
                    .id("refresh-mics")
                    .mt(rpx(4.))
                    .text_size(ts::BODY)
                    .text_color(p.stamp)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_devices(cx)))
                    .child("Refresh list"),
            );
        (modes.into_any_element(), mode_note, mics.into_any_element())
    }

    fn render_settings(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let s = &self.settings;
        let row = |label: &'static str, value: AnyElement| {
            div()
                .flex()
                .items_start()
                .gap(rpx(12.))
                .py(rpx(8.))
                .border_b_1()
                .border_color(p.bd)
                .child(
                    div()
                        .w(rpx(200.))
                        .flex_none()
                        .text_color(p.fg2)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().text_color(p.text).child(value))
        };
        let text = |t: String| div().child(t).into_any_element();

        let (modes, mode_note, mics) = self.audio_controls(p, cx);

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(screen_title(
                "Settings",
                Some("Changes save to the settings file as you make them."),
                p,
            ))
            .child(
                div()
                    .id("settings-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(rpx(20.))
                    .pb(rpx(20.))
                    .max_w(rpx(760.))
                    .text_size(ts::UI)
                    .child(ui::section_label("Audio", p.fg3).mt(rpx(4.)).mb(rpx(4.)))
                    .child(row(
                        "Teams capture",
                        div()
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap(rpx(6.))
                            .child(modes)
                            .child(div().text_size(ts::BODY).text_color(p.fg3).child(mode_note))
                            .into_any_element(),
                    ))
                    .child(row("Microphone", mics.into_any_element()))
                    .child(ui::section_label("Models", p.fg3).mt(rpx(18.)).mb(rpx(4.)))
                    .child(self.render_models(p, cx))
                    .child(ui::section_label("Agent", p.fg3).mt(rpx(18.)).mb(rpx(4.)))
                    .child(agent_view::settings_agent(self, p, cx))
                    .child(crate::detection::settings_detection(self, p, cx))
                    .child(crate::integrations_view::settings_integrations(self, p, cx))
                    .child(
                        ui::section_label("Everything else", p.fg3)
                            .mt(rpx(18.))
                            .mb(rpx(4.)),
                    )
                    .child(row(
                        "Updates",
                        crate::updates::settings_updates(self, p, cx),
                    ))
                    .child(row(
                        "Setup",
                        ui::button("run-setup", "Run setup again", Kind::Outline, p)
                            .on_click(cx.listener(|ws, _, w, cx| ws.restart_onboarding(w, cx)))
                            .into_any_element(),
                    ))
                    .child(row(
                        "Data folder",
                        text(if self.demo {
                            "Demo data in memory (nothing is saved)".to_owned()
                        } else {
                            self.paths.data.display().to_string()
                        }),
                    ))
                    .child(row(
                        "Settings file",
                        text(self.paths.settings_file().display().to_string()),
                    ))
                    .child(row(
                        "Keep audio",
                        div()
                            .flex()
                            .flex_col()
                            .gap(rpx(4.))
                            .child(
                                div().flex().child(
                                    ui::button(
                                        "keep-audio",
                                        s.storage.keep_audio_days.map_or_else(
                                            || "Forever".to_owned(),
                                            |d| format!("{d} days"),
                                        ),
                                        Kind::Outline,
                                        p,
                                    )
                                    .on_click(cx.listener(|ws, _, _, cx| ws.cycle_keep_audio(cx))),
                                ),
                            )
                            .child(div().text_size(ts::BODY).text_color(p.fg3).child(
                                "Audio of finished meetings older than this is deleted; \
                                 transcripts, notes and summaries stay.",
                            ))
                            .into_any_element(),
                    ))
                    .child(row(
                        "Consent notice",
                        text(s.storage.consent_notice.clone()),
                    )),
            )
            .into_any_element()
    }
}

/// A screen's heading row: a 20 px title and an optional note.
pub(crate) fn screen_title(
    title: &'static str,
    note: Option<&'static str>,
    p: &Palette,
) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .items_baseline()
        .gap(rpx(12.))
        .px(rpx(20.))
        .py(rpx(16.))
        .child(
            div()
                .text_size(ts::DISPLAY)
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .when_some(note, |d, note| {
            d.child(div().text_size(ts::BODY).text_color(p.fg3).child(note))
        })
        .into_any_element()
}

/// A search snippet with its matched terms highlighted.
fn snippet(text: &str, p: &Palette) -> AnyElement {
    use switchscribe_store::db::{MATCH_END, MATCH_START};
    let mut row = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_wrap()
        .line_height(rpx(19.))
        .text_color(p.fg_ctl);
    for (i, part) in text.split(MATCH_START).enumerate() {
        let (hit, rest) = match (i, part.split_once(MATCH_END)) {
            (0, _) | (_, None) => ("", part),
            (_, Some((hit, rest))) => (hit, rest),
        };
        if !hit.is_empty() {
            row = row.child(
                div()
                    .px(px(1.))
                    .rounded(px(2.))
                    .bg(p.stamp.opacity(0.22))
                    .text_color(p.fg)
                    .child(hit.to_owned()),
            );
        }
        for word in rest.split_inclusive(' ').filter(|w| !w.is_empty()) {
            row = row.child(word.replace(' ', "\u{a0}"));
        }
    }
    row.into_any_element()
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let title = self.render_title_bar(&p, cx);
        let sidebar = self
            .settings
            .layout
            .sidebar_open
            .then(|| self.render_sidebar(&p, cx));
        if self.onboarding.is_some() {
            let setup = self.render_onboarding(&p, cx);
            return div()
                .id("workspace")
                .key_context("Workspace")
                .track_focus(&self.focus)
                .size_full()
                .flex()
                .flex_col()
                .bg(p.bg)
                .text_color(p.fg)
                .font_family(SANS)
                .text_size(ts::BASE)
                .child(title)
                .child(div().flex_1().min_h_0().flex().child(setup));
        }
        let main = self.render_main(&p, cx);
        let ask = self.ask_open.then(|| {
            div()
                .w(rpx(360.))
                .flex_none()
                .min_h_0()
                .border_l_1()
                .border_color(p.bd)
                .child(self.ask.clone())
                .into_any_element()
        });
        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(p.bg)
            .text_color(p.fg)
            .font_family(SANS)
            .text_size(ts::BASE)
            .on_action(cx.listener(|this, _: &actions::ToggleAsk, w, cx| this.toggle_ask(w, cx)))
            .on_action(
                cx.listener(|this, _: &actions::ToggleSidebar, _, cx| this.toggle_sidebar(cx)),
            )
            .on_action(
                cx.listener(|this, _: &actions::SearchAll, w, cx| this.show(Screen::Search, w, cx)),
            )
            .on_action(cx.listener(|this, _: &actions::ShowSettings, w, cx| {
                this.show(Screen::Settings, w, cx)
            }))
            .on_action(
                cx.listener(|this, _: &actions::ToggleRecording, w, cx| {
                    this.toggle_recording(w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &actions::Bookmark, _, cx| this.bookmark(cx)))
            .child(title)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .children(sidebar)
                    .child(main)
                    .children(ask),
            )
    }
}
