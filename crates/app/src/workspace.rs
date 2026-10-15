//! The main window: title bar, meetings sidebar (260 px), the main pane and the Ask panel
//! (360 px), laid out like the prototype. The sidebar and the Ask panel collapse.

use std::sync::{Arc, Mutex};

use chrono::{Local, NaiveDate};
use gpui_kit::component::TitleBar;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, px, svg,
};
use switchscribe_store::{
    Meeting, MeetingStatus, Paths, Person, SearchHit, Segment, Settings, Store, group_by_date,
};

use crate::actions;
use crate::appearance::{rpx, ts};
use crate::ask_panel::{AskEvent, AskPanel};
use crate::meeting_view;
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind};

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
}

/// The main pane's screens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Screen {
    /// A meeting, by id.
    Meeting(String),
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
    store: SharedStore,
    pub(crate) demo: bool,
    store_error: Option<String>,
    pub(crate) meetings: Vec<Meeting>,
    pub(crate) people: Vec<Person>,
    pub(crate) today: NaiveDate,
    pub(crate) screen: Screen,
    pub(crate) tab: Tab,
    /// The loaded transcript: meeting id and its segments.
    pub(crate) transcript: Option<(String, Vec<Segment>)>,
    /// The cued moment of the selected meeting, seconds (no audio until M2).
    pub(crate) playhead: i64,
    ask: Entity<AskPanel>,
    ask_open: bool,
    search_input: Entity<InputState>,
    search_hits: Vec<SearchHit>,
    search_generation: u64,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl Workspace {
    /// The window's root view.
    pub fn new(s: Startup, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let ask = cx.new(|cx| AskPanel::new(s.settings.agent.cli, s.demo, window, cx));
        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search every meeting"));
        let subs = vec![
            cx.subscribe_in(&ask, window, |this, _, ev: &AskEvent, _, cx| match ev {
                AskEvent::Cite(at) => this.cue(*at, cx),
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
            ask,
            ask_open,
            search_input,
            search_hits: Vec::new(),
            search_generation: 0,
            focus: cx.focus_handle(),
            _subs: subs,
        };
        if this.demo {
            this.playhead = 236;
        }
        this.load_transcript(cx);
        this
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

    fn select(&mut self, id: String, cx: &mut Context<Self>) {
        if self.screen != Screen::Meeting(id.clone()) {
            self.screen = Screen::Meeting(id);
            self.playhead = 0;
            self.load_transcript(cx);
        }
        cx.notify();
    }

    fn show(&mut self, screen: Screen, window: &mut Window, cx: &mut Context<Self>) {
        let search = screen == Screen::Search;
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
        cx.notify();
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

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().to_string();
        self.search_generation += 1;
        let generation = self.search_generation;
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            match store.lock() {
                Ok(s) => s.search(&query, 200),
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
    fn save_settings(&self, cx: &mut Context<Self>) {
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

    fn cycle_cli(&mut self, cx: &mut Context<Self>) {
        let cli = self.settings.agent.cli.next();
        self.settings.agent.cli = cli;
        self.ask.update(cx, |a, cx| a.set_cli(cli, cx));
        self.save_settings(cx);
        cx.notify();
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
        let record = ui::button("record", "", Kind::Secondary, p)
            .occlude()
            .px(rpx(12.))
            .bg(p.elev)
            .text_color(p.fg)
            .font_weight(FontWeight::MEDIUM)
            .tooltip(|w, cx| {
                Tooltip::new("Recording arrives in M1")
                    .action(&actions::ToggleRecording, None)
                    .build(w, cx)
            })
            .child(
                div()
                    .size(rpx(8.))
                    .rounded_full()
                    .border(px(1.5))
                    .border_color(p.fg2),
            )
            .child("Record")
            .child(ui::kbd("Ctrl+Shift+R", p));
        let cli = ui::button("cli", "", Kind::Outline, p)
            .occlude()
            .gap(rpx(6.))
            .tooltip(|w, cx| Tooltip::new("Coding CLI for summaries and Ask").build(w, cx))
            .on_click(cx.listener(|this, _, _, cx| this.cycle_cli(cx)))
            .child(ui::dot(6., p.ok))
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
                    (Screen::ActionItems, "Action items", String::new()),
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
            Screen::Empty => self.render_empty(p, cx),
            Screen::ActionItems => self.render_placeholder(
                "Action items",
                "Action items from every meeting land here once summaries run (M4), with owners, due dates and Send to Jira (M5).",
                p,
            ),
            Screen::People => self.render_people(p),
            Screen::Search => self.render_search(p, cx),
            Screen::Settings => self.render_settings(p),
        };
        div()
            .flex_1()
            .min_w(rpx(480.))
            .flex()
            .flex_col()
            .bg(p.bg)
            .when_some(self.store_error.clone(), |d, e| {
                d.child(
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
                        .child(div().flex_1().child(format!(
                            "Could not open the meetings database ({e}). Nothing is being saved."
                        ))),
                )
            })
            .child(content)
            .into_any_element()
    }

    fn render_empty(&self, p: &Palette, _cx: &mut Context<Self>) -> AnyElement {
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
                        .tooltip(|w, cx| Tooltip::new("Recording arrives in M1").build(w, cx)),
                )
                .child(
                    div()
                        .id("test-recording")
                        .text_size(ts::BODY)
                        .text_color(p.stamp)
                        .cursor_pointer()
                        .tooltip(|w, cx| Tooltip::new("Recording arrives in M1").build(w, cx))
                        .child("Start a test recording"),
                ),
        )
        .into_any_element()
    }

    fn render_placeholder(
        &self,
        title: &'static str,
        body: &'static str,
        p: &Palette,
    ) -> AnyElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .child(screen_title(title, None, p))
            .child(ui::empty_state("Nothing here yet", body, p))
            .into_any_element()
    }

    fn render_people(&self, p: &Palette) -> AnyElement {
        let header = |text: &'static str| {
            div()
                .px(rpx(4.))
                .py(rpx(8.))
                .border_b_1()
                .border_color(p.bd)
                .text_size(ts::SMALL)
                .text_color(p.fg3)
                .child(text)
        };
        let rows = self.people.iter().map(|person| {
            div()
                .flex()
                .items_center()
                .gap(rpx(10.))
                .px(rpx(4.))
                .py(rpx(10.))
                .border_b_1()
                .border_color(p.elev)
                .when(person.is_me, |d| d.bg(p.surface))
                .child(
                    div()
                        .size(rpx(12.))
                        .rounded(px(3.))
                        .bg(p.speaker(person.color)),
                )
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(person.name.clone()),
                )
                .when(person.is_me, |d| {
                    d.child(
                        div()
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .child("you · mic track"),
                    )
                })
        });
        div()
            .flex_1()
            .flex()
            .flex_col()
            .child(screen_title(
                "People",
                Some("Voiceprints are stored encrypted on this machine and can be deleted per person."),
                p,
            ))
            .child(if self.people.is_empty() {
                ui::empty_state(
                    "No voiceprints yet",
                    "You enroll your own voice during onboarding; colleagues are added when you name a speaker (M3).",
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
                    .child(header("Name"))
                    .children(rows)
                    .into_any_element()
            })
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
                            .items_center()
                            .gap(rpx(6.))
                            .text_size(ts::BODY)
                            .text_color(p.fg4)
                            .child("Filters by person, date and meeting type arrive in M4.")
                            .child(div().flex_1())
                            .child(ui::mono(
                                format!("{} hits", self.search_hits.len()),
                                ts::BODY,
                                p.fg3,
                            )),
                    ),
            )
            .child(results)
            .into_any_element()
    }

    fn render_settings(&self, p: &Palette) -> AnyElement {
        let s = &self.settings;
        let row = |label: &'static str, value: String| {
            div()
                .flex()
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
        let on = |b: bool| if b { "On" } else { "Off" }.to_owned();
        div()
            .flex_1()
            .flex()
            .flex_col()
            .child(screen_title(
                "Settings",
                Some("Editing arrives with each feature; this is what the settings file holds."),
                p,
            ))
            .child(
                div()
                    .id("settings-rows")
                    .flex_1()
                    .overflow_y_scroll()
                    .px(rpx(20.))
                    .pb(rpx(20.))
                    .max_w(rpx(760.))
                    .text_size(ts::UI)
                    .child(row(
                        "Data folder",
                        if self.demo {
                            "Demo data in memory (nothing is saved)".to_owned()
                        } else {
                            self.paths.data.display().to_string()
                        },
                    ))
                    .child(row(
                        "Settings file",
                        self.paths.settings_file().display().to_string(),
                    ))
                    .child(row(
                        "Teams capture",
                        match s.audio.capture_mode {
                            switchscribe_store::CaptureMode::TeamsOnly => "Teams only".to_owned(),
                            switchscribe_store::CaptureMode::WholeSystem => {
                                "Whole system".to_owned()
                            }
                        },
                    ))
                    .child(row(
                        "Microphone",
                        s.audio
                            .mic_device
                            .clone()
                            .unwrap_or_else(|| "Default communications device".to_owned()),
                    ))
                    .child(row("Detect meetings", on(s.detection.auto_detect)))
                    .child(row(
                        "When detected",
                        if s.detection.auto_record {
                            "Record"
                        } else {
                            "Ask first"
                        }
                        .to_owned(),
                    ))
                    .child(row(
                        "Auto-stop after silence",
                        format!("{} min", s.detection.auto_stop_minutes),
                    ))
                    .child(row(
                        "Transcription model",
                        s.models
                            .transcription_model
                            .clone()
                            .unwrap_or_else(|| "None installed".to_owned()),
                    ))
                    .child(row("Use GPU", on(s.models.use_gpu)))
                    .child(row("Live captions", on(s.models.live_captions)))
                    .child(row(
                        "Coding CLI",
                        match &s.agent.cli_path {
                            Some(path) => format!("{} ({})", s.agent.cli.label(), path.display()),
                            None => format!(
                                "{} (`{}` on PATH)",
                                s.agent.cli.label(),
                                s.agent.cli.program()
                            ),
                        },
                    ))
                    .child(row(
                        "Keep audio",
                        s.storage
                            .keep_audio_days
                            .map_or_else(|| "Forever".to_owned(), |d| format!("{d} days")),
                    ))
                    .child(row("Consent notice", s.storage.consent_notice.clone())),
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
            .on_action(cx.listener(|_, _: &actions::ToggleRecording, _, _| {
                tracing::info!("recording arrives in M1");
            }))
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
