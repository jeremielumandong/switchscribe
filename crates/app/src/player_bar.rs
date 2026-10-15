//! The audio player docked under a meeting: play, time, speed, skip silence, and one
//! lane of bars per speaker with the playhead across them. Clicking the lanes seeks.
//!
//! Meetings without audio (the demo) get a silent clock, so the transcript still follows.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Bounds, Context, Hsla, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, ParentElement as _, Pixels, SharedString, StatefulInteractiveElement as _,
    Styled as _, canvas, div, px,
};
use switchscribe_capture::Player;
use switchscribe_store::{MeetingStatus, Segment};

use crate::appearance::{rpx, ts};
use crate::format;
use crate::theme::Palette;
use crate::ui;
use crate::workspace::{Tab, Workspace};

/// Bars per lane, as in the prototype.
pub const BARS: usize = 120;
/// The speed control's steps.
pub const SPEEDS: [f32; 4] = [1.0, 1.25, 1.5, 2.0];
/// At most this many lanes; quieter speakers share the last.
const MAX_LANES: usize = 6;

/// Keeps time for a meeting with no audio.
#[derive(Debug)]
struct Clock {
    base_ms: u64,
    started: Option<Instant>,
    speed: f32,
    total_ms: u64,
    skip: bool,
}

impl Clock {
    fn position_ms(&self) -> u64 {
        let run = self.started.map_or(0.0, |s| {
            s.elapsed().as_secs_f64() * 1000.0 * f64::from(self.speed)
        });
        (self.base_ms + run as u64).min(self.total_ms)
    }

    fn rebase(&mut self) {
        self.base_ms = self.position_ms();
        if self.started.is_some() {
            self.started = Some(Instant::now());
        }
    }
}

enum Source {
    Audio(Player),
    Clock(Clock),
}

/// The selected meeting's player.
pub struct Playback {
    /// Which meeting.
    pub meeting_id: String,
    source: Source,
    /// Loudness per bar, `0..=1`; empty without audio.
    envelope: Vec<f32>,
    /// Where the lanes were last drawn, for turning a click into a time.
    lanes: Rc<Cell<Bounds<Pixels>>>,
}

impl Playback {
    /// A player for real audio.
    pub fn audio(meeting_id: String, player: Player) -> Self {
        let envelope = player.envelope(BARS);
        Self {
            meeting_id,
            source: Source::Audio(player),
            envelope,
            lanes: Rc::default(),
        }
    }

    /// A silent clock `total_ms` long.
    pub fn clock(meeting_id: String, total_ms: u64) -> Self {
        Self {
            meeting_id,
            source: Source::Clock(Clock {
                base_ms: 0,
                started: None,
                speed: 1.0,
                total_ms,
                skip: false,
            }),
            envelope: Vec::new(),
            lanes: Rc::default(),
        }
    }

    /// Whether there is real audio behind it.
    pub fn has_audio(&self) -> bool {
        matches!(self.source, Source::Audio(_))
    }

    /// Length, milliseconds.
    pub fn duration_ms(&self) -> u64 {
        match &self.source {
            Source::Audio(p) => p.duration_ms(),
            Source::Clock(c) => c.total_ms,
        }
    }

    /// Position, milliseconds.
    pub fn position_ms(&self) -> u64 {
        match &self.source {
            Source::Audio(p) => p.position_ms(),
            Source::Clock(c) => c.position_ms(),
        }
    }

    /// Whether it is playing. A clock stops itself at the end.
    pub fn is_playing(&mut self) -> bool {
        match &mut self.source {
            Source::Audio(p) => p.is_playing(),
            Source::Clock(c) => {
                if c.started.is_some() && c.position_ms() >= c.total_ms {
                    c.base_ms = c.total_ms;
                    c.started = None;
                }
                c.started.is_some()
            }
        }
    }

    /// Play or pause.
    pub fn toggle(&mut self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Start playing (from the top when at the end).
    pub fn play(&mut self) {
        match &mut self.source {
            Source::Audio(p) => p.play(),
            Source::Clock(c) => {
                if c.position_ms() >= c.total_ms {
                    c.base_ms = 0;
                }
                c.started = Some(Instant::now());
            }
        }
    }

    /// Pause.
    pub fn pause(&mut self) {
        match &mut self.source {
            Source::Audio(p) => p.pause(),
            Source::Clock(c) => {
                c.base_ms = c.position_ms();
                c.started = None;
            }
        }
    }

    /// Jump to `ms`.
    pub fn seek_ms(&mut self, ms: u64) {
        match &mut self.source {
            Source::Audio(p) => p.seek_ms(ms),
            Source::Clock(c) => {
                c.base_ms = ms.min(c.total_ms);
                if c.started.is_some() {
                    c.started = Some(Instant::now());
                }
            }
        }
    }

    /// Speed.
    pub fn speed(&self) -> f32 {
        match &self.source {
            Source::Audio(p) => p.speed(),
            Source::Clock(c) => c.speed,
        }
    }

    /// Change speed.
    pub fn set_speed(&mut self, speed: f32) {
        match &mut self.source {
            Source::Audio(p) => p.set_speed(speed),
            Source::Clock(c) => {
                c.rebase();
                c.speed = speed;
            }
        }
    }

    /// Whether long silences are skipped.
    pub fn skip_silence(&self) -> bool {
        match &self.source {
            Source::Audio(p) => p.skip_silence(),
            Source::Clock(c) => c.skip,
        }
    }

    /// Turn silence skipping on or off (a clock has no silences to skip).
    pub fn set_skip_silence(&mut self, on: bool) {
        match &mut self.source {
            Source::Audio(p) => p.set_skip_silence(on),
            Source::Clock(c) => c.skip = on,
        }
    }
}

/// One lane: a speaker and the bars where they talk.
#[derive(Clone, Debug, PartialEq)]
pub struct Lane {
    /// Name on the left.
    pub name: SharedString,
    /// Dot and bar color.
    pub color: Hsla,
    /// Per bar: `None` when someone else (or no one) talks, else the bar's height `0..=1`.
    pub bars: Vec<Option<f32>>,
}

/// Lanes for a transcript: one per speaker in order of first appearance. `name_of` gives
/// a segment's speaker key, display name and color.
pub fn lanes(
    segments: &[Segment],
    total_ms: u64,
    envelope: &[f32],
    name_of: impl Fn(&Segment) -> (SharedString, Hsla),
) -> Vec<Lane> {
    let mut keys: Vec<SharedString> = Vec::new();
    let mut out: Vec<Lane> = Vec::new();
    let seg_lane: Vec<usize> = segments
        .iter()
        .map(|s| {
            let (name, color) = name_of(s);
            match keys.iter().position(|k| *k == name) {
                Some(i) => i.min(MAX_LANES - 1),
                None if keys.len() < MAX_LANES => {
                    keys.push(name.clone());
                    out.push(Lane {
                        name,
                        color,
                        bars: vec![None; BARS],
                    });
                    keys.len() - 1
                }
                None => MAX_LANES - 1,
            }
        })
        .collect();
    if total_ms == 0 {
        return out;
    }
    for b in 0..BARS {
        let t = ((b as f64 + 0.5) / BARS as f64 * total_ms as f64) as i64;
        let Some(i) = segments
            .iter()
            .position(|s| s.start_ms <= t && t < s.end_ms.max(s.start_ms + 1))
        else {
            continue;
        };
        let height = match envelope.get(b) {
            Some(level) => 0.3 + 0.7 * level,
            // No audio: a steady wobble so the lanes still read as speech.
            None => 0.3 + 0.7 * (((b * 7 + i * 13) % 17) as f32 / 16.0),
        };
        out[seg_lane[i]].bars[b] = Some(height);
    }
    out
}

/// The player bar.
pub fn render(ws: &Workspace, pb: &Playback, p: &Palette, cx: &mut Context<Workspace>) -> AnyElement {
    let pos = pb.position_ms();
    let total = pb.duration_ms().max(1);
    let playing = match &pb.source {
        Source::Audio(player) => player.is_playing(),
        Source::Clock(c) => c.started.is_some() && pos < c.total_ms,
    };
    let speed = pb.speed();
    let skip = pb.skip_silence();
    let segments: &[Segment] = match &ws.transcript {
        Some((id, segs)) if *id == pb.meeting_id => segs,
        _ => &[],
    };
    let mut lanes = lanes(segments, total, &pb.envelope, |s| {
        match s.person_id.and_then(|id| ws.person(id)) {
            Some(person) => (person.name.clone().into(), p.speaker(person.color)),
            None => (s.speaker_label.clone().into(), p.unknown),
        }
    });
    if lanes.is_empty() {
        // Not transcribed yet: one lane of the mixed audio.
        lanes.push(Lane {
            name: "Audio".into(),
            color: p.fg3,
            bars: pb
                .envelope
                .iter()
                .map(|v| (*v > 0.05).then_some(0.3 + 0.7 * v))
                .collect(),
        });
    }
    let played = pos as f32 / total as f32;

    let play = div()
        .id("player-play")
        .size(rpx(30.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(p.fg)
        .text_color(p.on_light)
        .text_size(ts::SMALL)
        .cursor_pointer()
        .on_click(cx.listener(|ws, _, _, cx| ws.toggle_playback(cx)))
        .child(if playing { "❚❚" } else { "▶" });
    let time = div()
        .flex()
        .gap(rpx(4.))
        .child(ui::mono(
            format::clock((pos / 1000) as i64),
            ts::BODY,
            p.fg_chip,
        ))
        .child(ui::mono(
            format!("/ {}", format::clock((total / 1000) as i64)),
            ts::BODY,
            p.fg4,
        ));
    let speeds = div()
        .flex()
        .flex_none()
        .border_1()
        .border_color(p.bd2)
        .rounded(px(6.))
        .overflow_hidden()
        .children(SPEEDS.iter().enumerate().map(|(i, &v)| {
            let on = (speed - v).abs() < 0.01;
            div()
                .id(("speed", i))
                .px(rpx(8.))
                .py(rpx(3.))
                .cursor_pointer()
                .when(on, |d| d.bg(p.bd2))
                .on_click(cx.listener(move |ws, _, _, cx| ws.set_playback_speed(v, cx)))
                .child(ui::mono(
                    format!("{}×", format_speed(v)),
                    ts::SMALL,
                    if on { p.fg } else { p.fg2 },
                ))
        }));
    let skip_box = div()
        .id("player-skip")
        .flex()
        .items_center()
        .gap(rpx(6.))
        .text_size(ts::BODY)
        .text_color(p.fg_ctl)
        .cursor_pointer()
        .when(!pb.has_audio(), |d| d.opacity(0.5))
        .on_click(cx.listener(move |ws, _, _, cx| ws.set_skip_silence(!skip, cx)))
        .child(
            div()
                .size(rpx(13.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .border_1()
                .border_color(if skip { p.fg } else { p.fg4 })
                .when(skip, |d| d.bg(p.fg))
                .when(skip, |d| {
                    d.child(div().text_color(p.on_light).text_size(ts::TINY).child("✓"))
                }),
        )
        .child("Skip silence");

    let names = div()
        .w(rpx(84.))
        .flex_none()
        .flex()
        .flex_col()
        .gap(rpx(3.))
        .children(lanes.iter().map(|l| {
            div()
                .h(rpx(14.))
                .flex()
                .items_center()
                .gap(rpx(5.))
                .text_size(ts::TINY)
                .text_color(p.fg2)
                .overflow_hidden()
                .child(ui::dot(6., l.color))
                .child(div().min_w_0().truncate().child(l.name.clone()))
        }));
    let bounds = pb.lanes.clone();
    let measure = {
        let bounds = bounds.clone();
        canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {})
            .absolute()
            .size_full()
    };
    let bars = div()
        .id("player-lanes")
        .flex_1()
        .min_w_0()
        .relative()
        .flex()
        .flex_col()
        .gap(rpx(3.))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, ev: &MouseDownEvent, _, cx| {
                let b = bounds.get();
                let w = f32::from(b.size.width);
                if w > 0.0 {
                    let frac = (f32::from(ev.position.x - b.origin.x) / w).clamp(0.0, 1.0);
                    ws.seek_playback((frac as f64 * total as f64) as u64, cx);
                }
            }),
        )
        .child(measure)
        .children(lanes.iter().map(|l| {
            div()
                .h(rpx(14.))
                .flex()
                .items_center()
                .gap(px(1.))
                .children(l.bars.iter().enumerate().map(|(b, bar)| {
                    let t = (b as f32 + 0.5) / BARS as f32;
                    let (h, color, opacity) = match bar {
                        Some(h) => (*h, l.color, if t <= played { 1.0 } else { 0.55 }),
                        None => (0.14, p.bd2, 1.0),
                    };
                    div()
                        .flex_1()
                        .h(gpui_kit::relative(h))
                        .rounded(px(1.))
                        .bg(color)
                        .opacity(opacity)
                }))
        }))
        .child(
            div()
                .absolute()
                .top(px(-3.))
                .bottom(px(-3.))
                .left(gpui_kit::relative(played.clamp(0.0, 1.0)))
                .w(px(1.5))
                .bg(p.fg),
        );

    div()
        .flex_none()
        .border_t_1()
        .border_color(p.bd)
        .bg(p.panel)
        .px(rpx(16.))
        .pt(rpx(10.))
        .pb(rpx(12.))
        .flex()
        .flex_col()
        .gap(rpx(8.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(rpx(12.))
                .child(play)
                .child(time)
                .child(div().flex_1())
                .child(speeds)
                .child(skip_box),
        )
        .child(div().flex().gap(rpx(10.)).child(names).child(bars))
        .into_any_element()
}

impl Workspace {
    /// Open the selected meeting's audio off the UI thread; meetings without audio get a
    /// silent clock as long as the meeting.
    pub(crate) fn load_playback(&mut self, cx: &mut Context<Self>) {
        let Some(m) = self.selected() else {
            self.playback = None;
            return;
        };
        if self.playback.as_ref().is_some_and(|pb| pb.meeting_id == m.id) {
            return;
        }
        let id = m.id.clone();
        let total_ms = u64::from(m.duration_secs) * 1000;
        let recording = m.status == MeetingStatus::Recording;
        self.playback = None;
        if recording {
            return;
        }
        let dir = self.meeting_dir(&id);
        let demo = self.demo;
        let task = cx.background_executor().spawn(async move {
            // The demo's meetings have no audio; skip the lookup.
            if demo && !dir.exists() {
                return None;
            }
            match Player::open(&dir) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::debug!(error = %e, "no audio to play");
                    None
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let player = task.await;
            let _ = this.update(cx, |ws, cx| {
                if ws.selected().is_none_or(|m| m.id != id) {
                    return;
                }
                ws.playback = Some(match player {
                    Some(p) => Playback::audio(id, p),
                    None => Playback::clock(id, total_ms),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Play or pause the selected meeting.
    pub(crate) fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        let Some(pb) = &mut self.playback else {
            return;
        };
        pb.toggle();
        self.follow_playback(cx);
        cx.notify();
    }

    /// Play from `at` seconds.
    pub(crate) fn play_from(&mut self, at: i64, cx: &mut Context<Self>) {
        self.cue(at, cx);
        if let Some(pb) = &mut self.playback {
            pb.play();
        }
        self.follow_playback(cx);
    }

    /// Jump to `ms`.
    pub(crate) fn seek_playback(&mut self, ms: u64, cx: &mut Context<Self>) {
        if let Some(pb) = &mut self.playback {
            pb.seek_ms(ms);
            self.playhead = (ms / 1000) as i64;
            self.scroll_to_playhead();
        }
        cx.notify();
    }

    /// Change the speed.
    pub(crate) fn set_playback_speed(&mut self, speed: f32, cx: &mut Context<Self>) {
        if let Some(pb) = &mut self.playback {
            pb.set_speed(speed);
        }
        cx.notify();
    }

    /// Turn silence skipping on or off.
    pub(crate) fn set_skip_silence(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(pb) = &mut self.playback {
            pb.set_skip_silence(on);
        }
        cx.notify();
    }

    /// While playing, move the playhead and keep the active line in view.
    fn follow_playback(&mut self, cx: &mut Context<Self>) {
        if self.following {
            return;
        }
        self.following = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let playing = this.update(cx, |ws, cx| {
                    let Some(pb) = &mut ws.playback else {
                        ws.following = false;
                        return false;
                    };
                    let playing = pb.is_playing();
                    let secs = (pb.position_ms() / 1000) as i64;
                    if secs != ws.playhead {
                        ws.playhead = secs;
                        ws.scroll_to_playhead();
                    }
                    if !playing {
                        ws.following = false;
                    }
                    cx.notify();
                    playing
                });
                if !playing.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Scroll the transcript to the line at the playhead.
    pub(crate) fn scroll_to_playhead(&self) {
        if self.tab != Tab::Transcript {
            return;
        }
        if let Some((_, segs)) = &self.transcript
            && let Some(i) = active_segment(segs, self.playhead)
        {
            self.body_scroll.scroll_to_item(i);
        }
    }
}

/// The transcript line playing at `secs`: the last one started by then.
pub fn active_segment(segments: &[Segment], secs: i64) -> Option<usize> {
    segments.iter().rposition(|s| s.start_ms <= secs * 1000)
}

/// `1`, `1.25`, `1.5`, `2`.
fn format_speed(v: f32) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start_ms: i64, end_ms: i64, label: &str) -> Segment {
        Segment {
            id: 0,
            start_ms,
            end_ms,
            person_id: None,
            speaker_label: label.into(),
            text: String::new(),
            low_confidence: false,
        }
    }

    #[test]
    fn lanes_follow_speakers() {
        let segs = [seg(0, 6_000, "You"), seg(6_000, 12_000, "Teams")];
        let white = gpui_kit::white();
        let l = lanes(&segs, 12_000, &[], |s| (s.speaker_label.clone().into(), white));
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].name.as_ref(), "You");
        // The first half of the bars are "You", the second half "Teams".
        assert!(l[0].bars[..BARS / 2].iter().all(Option::is_some));
        assert!(l[0].bars[BARS / 2..].iter().all(Option::is_none));
        assert!(l[1].bars[BARS / 2..].iter().all(Option::is_some));
        // With audio, heights follow the envelope.
        let env = vec![1.0; BARS];
        let l = lanes(&segs, 12_000, &env, |s| (s.speaker_label.clone().into(), white));
        assert_eq!(l[0].bars[0], Some(1.0));
    }

    #[test]
    fn clock_keeps_time() {
        let mut pb = Playback::clock("m".into(), 10_000);
        assert!(!pb.is_playing());
        pb.seek_ms(4_000);
        assert_eq!(pb.position_ms(), 4_000);
        pb.set_speed(2.0);
        pb.play();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let pos = pb.position_ms();
        assert!((4_150..4_600).contains(&pos), "{pos}");
        pb.pause();
        let paused = pb.position_ms();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(pb.position_ms(), paused);
        pb.seek_ms(20_000);
        assert_eq!(pb.position_ms(), 10_000);
        pb.play();
        assert!(pb.position_ms() < 100);
    }

    #[test]
    fn speeds_read_plainly() {
        let labels: Vec<String> = SPEEDS.iter().map(|v| format_speed(*v)).collect();
        assert_eq!(labels, ["1", "1.25", "1.5", "2"]);
    }
}
