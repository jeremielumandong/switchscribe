//! After the call: the transcription queue and model downloads. Each job runs on its own
//! thread; the workspace polls them a few times a second for progress and results, so the
//! UI thread never waits on a model.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use switchscribe_capture::RecorderState;
use switchscribe_pipeline::JobInput;
use switchscribe_pipeline::models::{
    self, EMBEDDING_MODEL, MODELS, ModelInfo, SEGMENTATION_MODEL, SUPPORT_MODELS, VAD_MODEL,
};
use switchscribe_store::MeetingStatus;

use crate::appearance::{rpx, ts};
use crate::recording::Banner;
use crate::speakers;
use crate::theme::Palette;
use crate::ui::{self, Kind};
use crate::workspace::{Screen, Workspace};

/// A job on its own thread: its progress, a way to cancel it, and its result.
pub struct Worker<T> {
    rx: mpsc::Receiver<T>,
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
}

/// What a [`Worker`] has to say.
pub enum Poll<T> {
    /// Still running.
    Pending,
    /// Finished.
    Done(T),
    /// The thread died without a result (it panicked, or never started).
    Lost,
}

impl<T: Send + 'static> Worker<T> {
    /// Run `job` on a new thread. It reports progress as `0.0..=1.0` and should stop
    /// soon after the flag it is given is set.
    pub fn spawn(
        name: &str,
        job: impl FnOnce(&dyn Fn(f32), &AtomicBool) -> T + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (p, c) = (progress.clone(), cancel.clone());
        let spawned = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let report = |f: f32| p.store(f.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
                let _ = tx.send(job(&report, &c));
            });
        if let Err(e) = spawned {
            tracing::error!("could not start {name}: {e}");
        }
        Self {
            rx,
            progress,
            cancel,
        }
    }

    /// Progress, `0.0..=1.0`.
    pub fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(Ordering::Relaxed))
    }

    /// Ask the job to stop.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Check for the result without waiting.
    pub fn poll(&self) -> Poll<T> {
        match self.rx.try_recv() {
            Ok(v) => Poll::Done(v),
            Err(mpsc::TryRecvError::Empty) => Poll::Pending,
            Err(mpsc::TryRecvError::Disconnected) => Poll::Lost,
        }
    }
}

/// Where a meeting is in transcription, for the stepper.
#[derive(Clone, Debug, PartialEq)]
pub enum Transcription {
    /// Waiting its turn.
    Queued,
    /// Waiting for a model to be downloaded.
    NeedsModel,
    /// Transcribing.
    Running(f32),
    /// Identifying speakers (transcription is done).
    Identifying(f32),
    /// Failed; the reason.
    Failed(String),
}

/// The workspace's background work.
#[derive(Default)]
pub struct Jobs {
    queue: VecDeque<String>,
    running: Option<Running>,
    failed: HashMap<String, String>,
    downloads: Vec<(&'static str, Worker<Result<(), String>>)>,
    /// Model ids in the models folder (the VAD model's too).
    pub installed: HashSet<&'static str>,
    polling: bool,
}

/// The meeting being processed.
struct Running {
    id: String,
    worker: Worker<Result<MeetingStatus, String>>,
    /// Speakers are identified after transcription (the speaker models are installed).
    identify: bool,
}

/// The share of a job's progress that transcription takes when speakers are identified
/// after it.
const TRANSCRIBE_SHARE: f32 = 0.8;

/// The download id of the speaker models (segmentation and embeddings, fetched together).
const SPEAKER_MODELS: &str = "speakers";

/// The installed models in `dir`, by id.
pub fn scan_models(dir: &Path) -> HashSet<&'static str> {
    MODELS
        .iter()
        .chain(SUPPORT_MODELS.iter())
        .filter(|m| m.is_installed(dir))
        .map(|m| m.id)
        .collect()
}

fn needs_model_banner() -> Banner {
    Banner {
        text: "No transcription model is installed, so transcription is paused.".into(),
        action: Some(("Download a model", Screen::Settings)),
    }
}

impl Jobs {
    /// No jobs yet; `installed` from the startup scan.
    pub fn new(installed: HashSet<&'static str>) -> Self {
        Self {
            installed,
            ..Self::default()
        }
    }

    /// A meeting's transcription state; `None` when it is not waiting or running.
    pub fn transcription(&self, id: &str) -> Option<Transcription> {
        if let Some(r) = &self.running
            && r.id == id
        {
            let f = r.worker.progress();
            return Some(if !r.identify {
                Transcription::Running(f)
            } else if f < TRANSCRIBE_SHARE {
                Transcription::Running(f / TRANSCRIBE_SHARE)
            } else {
                Transcription::Identifying((f - TRANSCRIBE_SHARE) / (1.0 - TRANSCRIBE_SHARE))
            });
        }
        if let Some(e) = self.failed.get(id) {
            return Some(Transcription::Failed(e.clone()));
        }
        if self.queue.iter().any(|q| q == id) {
            let blocked = !MODELS.iter().any(|m| self.installed.contains(m.id));
            return Some(if blocked {
                Transcription::NeedsModel
            } else {
                Transcription::Queued
            });
        }
        None
    }

    /// Download progress for a model, if it is downloading.
    pub fn download(&self, id: &str) -> Option<f32> {
        self.downloads
            .iter()
            .find(|(d, _)| *d == id)
            .map(|(_, w)| w.progress())
    }
}

impl Workspace {
    /// The model transcription uses: the chosen one if installed, else the best installed.
    fn transcription_model(&self) -> Option<&'static ModelInfo> {
        let installed = &self.jobs.installed;
        self.settings
            .models
            .transcription_model
            .as_deref()
            .and_then(models::find)
            .filter(|m| installed.contains(m.id))
            .or_else(|| MODELS.iter().rev().find(|m| installed.contains(m.id)))
    }

    /// Queue a recorded meeting for transcription.
    pub(crate) fn queue_transcription(&mut self, id: String, cx: &mut Context<Self>) {
        self.jobs.failed.remove(&id);
        let running = self.jobs.running.as_ref().is_some_and(|r| r.id == id);
        if !running && !self.jobs.queue.contains(&id) {
            self.jobs.queue.push_back(id);
        }
        self.pump(cx);
    }

    /// Start the next transcription if nothing is running.
    fn pump(&mut self, cx: &mut Context<Self>) {
        if self.jobs.running.is_some() || self.jobs.queue.is_empty() {
            return;
        }
        let Some(model) = self.transcription_model() else {
            let banner = needs_model_banner();
            if !self.banners.contains(&banner) {
                self.banners.push(banner);
            }
            cx.notify();
            return;
        };
        self.banners.retain(|b| *b != needs_model_banner());
        let Some(id) = self.jobs.queue.pop_front() else {
            return;
        };
        let models_dir = self.paths.models_dir();
        let model_path = model.path(&models_dir);
        let vad_path = self
            .jobs
            .installed
            .contains(VAD_MODEL.id)
            .then(|| VAD_MODEL.path(&models_dir));
        let use_gpu = self.settings.models.use_gpu;
        let meeting = self.meetings.iter().find(|m| m.id == id);
        // Names and the title help the model spell what it hears.
        let prompt = meeting
            .map(|m| {
                let mut words = vec![m.title.clone()];
                words.extend(m.attendees.iter().map(|a| a.name.clone()));
                words.join(", ")
            })
            .unwrap_or_default();
        let input = JobInput {
            me: self.people.iter().find(|p| p.is_me).map(|p| p.id),
            prompt,
            language: None,
        };
        let dir = self.meeting_dir(&id);
        let store = self.store.clone();
        let meeting_id = id.clone();
        let identify = speakers::models_installed(&self.jobs.installed);
        let share = if identify { TRANSCRIBE_SHARE } else { 1.0 };
        tracing::info!(meeting = %id, model = model.id, identify, "transcribing");
        let worker = Worker::spawn("transcribe", move |progress, cancel| {
            let segments = switchscribe_pipeline::whisper::transcribe_with_whisper(
                &dir,
                &model_path,
                vad_path.as_deref(),
                use_gpu,
                &input,
                &mut |f| progress(f * share),
                cancel,
            )
            .map_err(|e| e.to_string())?;
            {
                let mut store = store
                    .lock()
                    .map_err(|_| "the index is unavailable".to_owned())?;
                store
                    .replace_segments(&meeting_id, &segments)
                    .and_then(|()| store.set_status(&meeting_id, MeetingStatus::Ready))
                    .map_err(|e| e.to_string())?;
            }
            if !identify {
                return Ok(MeetingStatus::Ready);
            }
            let to_overall = |f: f32| progress(share + f * (1.0 - share));
            match speakers::identify_meeting(
                &dir,
                &models_dir,
                &store,
                &meeting_id,
                &to_overall,
                cancel,
            ) {
                Ok(status) => Ok(status),
                // The transcript stands; its Teams lines just stay unnamed.
                Err(e) => {
                    tracing::warn!(meeting = %meeting_id, error = %e, "could not identify speakers");
                    Ok(MeetingStatus::Ready)
                }
            }
        });
        self.jobs.running = Some(Running {
            id,
            worker,
            identify,
        });
        self.set_tray(RecorderState::Processing);
        self.poll_jobs(cx);
    }

    /// Download the speaker models (and the VAD model, if missing).
    pub(crate) fn download_speaker_models(&mut self, cx: &mut Context<Self>) {
        if self.jobs.download(SPEAKER_MODELS).is_some() {
            return;
        }
        let dir = self.paths.models_dir();
        let missing: Vec<ModelInfo> = SUPPORT_MODELS
            .iter()
            .filter(|m| !self.jobs.installed.contains(m.id))
            .copied()
            .collect();
        let total: u64 = missing
            .iter()
            .map(|m| u64::from(m.size_mb))
            .sum::<u64>()
            .max(1);
        let worker = Worker::spawn("download", move |progress, cancel| {
            let mut before = 0u64;
            for m in &missing {
                let size = u64::from(m.size_mb);
                models::download(
                    m,
                    &dir,
                    &mut |done, all| {
                        let f = done as f32 / all.unwrap_or(size * 1_000_000).max(1) as f32;
                        progress((before as f32 + f * size as f32) / total as f32);
                    },
                    cancel,
                )
                .map_err(|e| e.to_string())?;
                before += size;
            }
            Ok(())
        });
        self.jobs.downloads.push((SPEAKER_MODELS, worker));
        self.poll_jobs(cx);
        cx.notify();
    }

    /// Delete the speaker models.
    pub(crate) fn remove_speaker_models(&mut self, cx: &mut Context<Self>) {
        let dir = self.paths.models_dir();
        let task = cx.background_executor().spawn(async move {
            [SEGMENTATION_MODEL, EMBEDDING_MODEL]
                .iter()
                .try_for_each(|m| models::remove(m, &dir))
        });
        cx.spawn(async move |this, cx| {
            let removed = task.await;
            let _ = this.update(cx, |ws, cx| {
                if let Err(e) = removed {
                    ws.banners.push(Banner::plain(format!(
                        "Could not remove the speaker models: {e}"
                    )));
                }
                ws.refresh_installed(cx);
            });
        })
        .detach();
    }

    /// Download a model (and the VAD and speaker models, the first time).
    pub(crate) fn download_model(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(model) = models::find(id).copied() else {
            return;
        };
        if self.jobs.download(id).is_some() {
            return;
        }
        let dir = self.paths.models_dir();
        let support: Vec<ModelInfo> = SUPPORT_MODELS
            .iter()
            .filter(|m| !self.jobs.installed.contains(m.id))
            .copied()
            .collect();
        let worker = Worker::spawn("download", move |progress, cancel| {
            for m in &support {
                if let Err(e) = models::download(m, &dir, &mut |_, _| {}, cancel) {
                    // Transcription still works without them: speech is found by level,
                    // and Teams voices stay unnamed.
                    tracing::warn!("could not download {}: {e}", m.label);
                }
            }
            let size = u64::from(model.size_mb) * 1_000_000;
            models::download(
                &model,
                &dir,
                &mut |done, total| progress(done as f32 / total.unwrap_or(size).max(1) as f32),
                cancel,
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        self.jobs.downloads.push((id, worker));
        self.poll_jobs(cx);
        cx.notify();
    }

    /// Stop a download; the partial file is deleted.
    pub(crate) fn cancel_download(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if let Some((_, w)) = self.jobs.downloads.iter().find(|(d, _)| *d == id) {
            w.cancel();
        }
        cx.notify();
    }

    /// Delete a model from the models folder, off the UI thread.
    pub(crate) fn remove_model(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(model) = models::find(id).copied() else {
            return;
        };
        let dir = self.paths.models_dir();
        let task = cx
            .background_executor()
            .spawn(async move { models::remove(&model, &dir) });
        cx.spawn(async move |this, cx| {
            let removed = task.await;
            let _ = this.update(cx, |ws, cx| {
                match removed {
                    Ok(()) => {
                        ws.jobs.installed.remove(id);
                        if ws.settings.models.transcription_model.as_deref() == Some(id) {
                            ws.settings.models.transcription_model = None;
                            ws.save_settings(cx);
                        }
                    }
                    Err(e) => ws
                        .banners
                        .push(Banner::plain(format!("Could not remove the model: {e}"))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Transcribe with this model from now on.
    pub(crate) fn use_model(&mut self, id: &'static str, cx: &mut Context<Self>) {
        self.settings.models.transcription_model = Some(id.to_owned());
        self.save_settings(cx);
        cx.notify();
    }

    /// Turn the GPU on or off for transcription.
    pub(crate) fn toggle_gpu(&mut self, cx: &mut Context<Self>) {
        self.settings.models.use_gpu = !self.settings.models.use_gpu;
        self.save_settings(cx);
        cx.notify();
    }

    /// Turn live captions on or off, for the running recording too.
    pub(crate) fn toggle_live_captions(&mut self, cx: &mut Context<Self>) {
        self.settings.models.live_captions = !self.settings.models.live_captions;
        self.save_settings(cx);
        let started = self
            .recording
            .as_ref()
            .filter(|s| s.captions.is_none())
            .and_then(|s| s.recorder.as_ref())
            .map(|r| self.start_captions(r));
        if let Some(s) = self.recording.as_mut() {
            match started {
                Some(Ok(live)) => s.captions = live,
                Some(Err(banner)) => self.banners.push(banner),
                None if !self.settings.models.live_captions => s.captions = None,
                None => {}
            }
        }
        cx.notify();
    }

    /// Check running jobs, and keep checking while any run.
    fn poll_jobs(&mut self, cx: &mut Context<Self>) {
        if self.jobs.polling {
            return;
        }
        self.jobs.polling = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let busy = this.update(cx, |ws, cx| {
                    ws.collect_jobs(cx);
                    let busy = ws.jobs.running.is_some() || !ws.jobs.downloads.is_empty();
                    if !busy {
                        ws.jobs.polling = false;
                    }
                    cx.notify();
                    busy
                });
                if !busy.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Act on finished jobs.
    fn collect_jobs(&mut self, cx: &mut Context<Self>) {
        if let Some(Running { id, worker, .. }) = &self.jobs.running {
            let result = match worker.poll() {
                Poll::Pending => None,
                Poll::Done(r) => Some(r),
                Poll::Lost => Some(Err("the transcription stopped unexpectedly".into())),
            };
            if let Some(result) = result {
                let id = id.clone();
                self.jobs.running = None;
                self.transcription_finished(id, result, cx);
            }
        }
        let mut finished = Vec::new();
        self.jobs.downloads.retain(|(id, w)| match w.poll() {
            Poll::Pending => true,
            Poll::Done(r) => {
                finished.push((*id, r));
                false
            }
            Poll::Lost => {
                finished.push((*id, Err("the download stopped unexpectedly".into())));
                false
            }
        });
        for (id, result) in finished {
            match result {
                Ok(()) if id == SPEAKER_MODELS => self.refresh_installed(cx),
                Ok(()) => {
                    self.jobs.installed.insert(id);
                    let chosen = self.settings.models.transcription_model.as_deref();
                    if !chosen.is_some_and(|c| self.jobs.installed.contains(c)) {
                        self.use_model(id, cx);
                    }
                    // The VAD download may have failed on its own; check the folder.
                    self.refresh_installed(cx);
                    self.pump(cx);
                }
                Err(e) if e.contains("cancelled") => {}
                Err(e) => {
                    let label = if id == SPEAKER_MODELS {
                        "the speaker models"
                    } else {
                        models::find(id).map_or(id, |m| m.label)
                    };
                    self.banners
                        .push(Banner::plain(format!("Could not download {label}: {e}")));
                }
            }
        }
    }

    fn transcription_finished(
        &mut self,
        id: String,
        result: Result<MeetingStatus, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(status) => {
                tracing::info!(meeting = %id, status = status.as_str(), "processed");
                if let Some(m) = self.meetings.iter_mut().find(|m| m.id == id) {
                    m.status = status;
                }
                if self.selected().is_some_and(|m| m.id == id) {
                    self.reload_transcript(cx);
                    self.reload_speakers(cx);
                }
                // The user's voiceprint may have grown.
                self.refresh_library(cx);
                if self.settings.agent.auto_summarize {
                    self.queue_summary(id.clone(), cx);
                }
            }
            Err(e) => {
                tracing::warn!(meeting = %id, error = %e, "transcription failed");
                self.jobs.failed.insert(id, e);
            }
        }
        if self.recording.is_none() {
            self.set_tray(RecorderState::Idle);
        }
        self.pump(cx);
    }

    /// Look at the models folder again, off the UI thread.
    fn refresh_installed(&mut self, cx: &mut Context<Self>) {
        let dir = self.paths.models_dir();
        let task = cx
            .background_executor()
            .spawn(async move { scan_models(&dir) });
        cx.spawn(async move |this, cx| {
            let installed = task.await;
            let _ = this.update(cx, |ws, cx| {
                ws.jobs.installed = installed;
                cx.notify();
            });
        })
        .detach();
    }
}

impl Workspace {
    /// Settings → Models: the catalog with download, use and remove, and the GPU switch.
    pub(crate) fn render_models(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let chosen = self.transcription_model().map(|m| m.id);
        let recommended = models::recommended(self.settings.models.use_gpu).id;
        let link = |id: (&'static str, usize), label: &'static str| {
            div()
                .id(id)
                .text_size(ts::BODY)
                .text_color(p.stamp)
                .cursor_pointer()
                .child(label)
        };
        let rows = MODELS.iter().enumerate().map(|(i, m)| {
            let id = m.id;
            let installed = self.jobs.installed.contains(id);
            let actions: AnyElement = if let Some(f) = self.jobs.download(id) {
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .child(
                        div()
                            .w(rpx(90.))
                            .h(rpx(4.))
                            .rounded(px(2.))
                            .bg(p.bd2)
                            .overflow_hidden()
                            .child(div().h_full().w(gpui_kit::relative(f)).bg(p.fg)),
                    )
                    .child(ui::mono(format!("{:.0}%", f * 100.0), ts::SMALL, p.fg2))
                    .child(
                        link(("model-cancel", i), "Cancel")
                            .on_click(cx.listener(move |ws, _, _, cx| ws.cancel_download(id, cx))),
                    )
                    .into_any_element()
            } else if installed {
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(12.))
                    .child(if chosen == Some(id) {
                        div()
                            .px(rpx(7.))
                            .py(px(1.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(p.ok)
                            .text_size(ts::SMALL)
                            .text_color(p.ok)
                            .child("In use")
                            .into_any_element()
                    } else {
                        ui::button(("model-use", i), "Use", Kind::Secondary, p)
                            .on_click(cx.listener(move |ws, _, _, cx| ws.use_model(id, cx)))
                            .into_any_element()
                    })
                    .child(
                        link(("model-remove", i), "Remove")
                            .on_click(cx.listener(move |ws, _, _, cx| ws.remove_model(id, cx))),
                    )
                    .into_any_element()
            } else {
                ui::button(("model-download", i), "Download", Kind::Secondary, p)
                    .on_click(cx.listener(move |ws, _, _, cx| ws.download_model(id, cx)))
                    .into_any_element()
            };
            div()
                .flex()
                .items_center()
                .gap(rpx(12.))
                .py(rpx(8.))
                .border_b_1()
                .border_color(p.bd)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(rpx(8.))
                                .child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(p.fg)
                                        .child(m.label),
                                )
                                .when(id == recommended, |d| {
                                    d.child(ui::mono("recommended", ts::SMALL, p.fg3))
                                }),
                        )
                        .child(div().text_size(ts::BODY).text_color(p.fg3).child(m.note)),
                )
                .child(div().w(rpx(70.)).flex_none().child(ui::mono(
                    format!("{} MB", m.size_mb),
                    ts::SMALL,
                    p.fg2,
                )))
                .child(
                    div()
                        .w(rpx(190.))
                        .flex_none()
                        .flex()
                        .justify_end()
                        .child(actions),
                )
        });
        let speaker_size = SEGMENTATION_MODEL.size_mb + EMBEDDING_MODEL.size_mb;
        let speaker_actions: AnyElement =
            if let Some(f) = self.jobs.download(SPEAKER_MODELS) {
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .child(ui::mono(format!("{:.0}%", f * 100.0), ts::SMALL, p.fg2))
                    .child(link(("speaker-cancel", 0), "Cancel").on_click(
                        cx.listener(|ws, _, _, cx| ws.cancel_download(SPEAKER_MODELS, cx)),
                    ))
                    .into_any_element()
            } else if speakers::models_installed(&self.jobs.installed) {
                link(("speaker-remove", 0), "Remove")
                    .on_click(cx.listener(|ws, _, _, cx| ws.remove_speaker_models(cx)))
                    .into_any_element()
            } else {
                ui::button("speaker-download", "Download", Kind::Secondary, p)
                    .on_click(cx.listener(|ws, _, _, cx| ws.download_speaker_models(cx)))
                    .into_any_element()
            };
        let speaker_row = div()
            .flex()
            .items_center()
            .gap(rpx(12.))
            .py(rpx(8.))
            .border_b_1()
            .border_color(p.bd)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.fg)
                            .child("Speaker identification"),
                    )
                    .child(div().text_size(ts::BODY).text_color(p.fg3).child(
                        "pyannote segmentation and TitaNet voiceprints: tells Teams voices apart and recognizes people you have named.",
                    )),
            )
            .child(div().w(rpx(70.)).flex_none().child(ui::mono(
                format!("{speaker_size} MB"),
                ts::SMALL,
                p.fg2,
            )))
            .child(
                div()
                    .w(rpx(190.))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .child(speaker_actions),
            );
        let gpu = self.settings.models.use_gpu;
        let gpu_row = div()
            .flex()
            .items_center()
            .gap(rpx(12.))
            .py(rpx(8.))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_color(p.fg).child("Use the GPU"))
                    .child(div().text_size(ts::BODY).text_color(p.fg3).child(
                        "Builds with a GPU backend use the graphics card when this is on; others run on the CPU.",
                    )),
            )
            .child(
                ui::button("gpu", if gpu { "On" } else { "Off" }, Kind::Outline, p)
                    .on_click(cx.listener(|ws, _, _, cx| ws.toggle_gpu(cx))),
            );
        let captions = self.settings.models.live_captions;
        let live_note = match crate::captions::live_model(&self.jobs.installed, gpu) {
            Some(m) => format!(
                "Shows what is said while recording, from the {} model. The transcript after the call replaces them.",
                m.label
            ),
            None => "Shows what is said while recording. Needs the Base or Small model.".into(),
        };
        let captions_row = div()
            .flex()
            .items_center()
            .gap(rpx(12.))
            .py(rpx(8.))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_color(p.fg).child("Live captions"))
                    .child(div().text_size(ts::BODY).text_color(p.fg3).child(live_note)),
            )
            .child(
                ui::button("live-captions", if captions { "On" } else { "Off" }, Kind::Outline, p)
                    .on_click(cx.listener(|ws, _, _, cx| ws.toggle_live_captions(cx))),
            );
        div()
            .flex()
            .flex_col()
            .child(div().text_size(ts::BODY).text_color(p.fg3).pb(rpx(4.)).child(format!(
                "Models live in {}. The first download also fetches the Silero speech detector and the speaker models.",
                self.paths.models_dir().display()
            )))
            .children(rows)
            .child(speaker_row)
            .child(gpu_row)
            .child(captions_row)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_report_progress_and_results() {
        let w = Worker::spawn("test", |progress, cancel| {
            progress(0.5);
            while !cancel.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            42
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(matches!(w.poll(), Poll::Pending));
        assert_eq!(w.progress(), 0.5);
        w.cancel();
        let end = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match w.poll() {
                Poll::Done(v) => break assert_eq!(v, 42),
                Poll::Pending if std::time::Instant::now() < end => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                _ => panic!("no result"),
            }
        }
    }

    #[test]
    fn a_panicking_worker_is_lost() {
        let w: Worker<()> = Worker::spawn("test", |_, _| panic!("boom"));
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < end {
            if matches!(w.poll(), Poll::Lost) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("never reported lost");
    }

    #[test]
    fn transcription_states() {
        let mut jobs = Jobs::default();
        jobs.queue.push_back("a".into());
        assert_eq!(jobs.transcription("a"), Some(Transcription::NeedsModel));
        jobs.installed.insert("small");
        assert_eq!(jobs.transcription("a"), Some(Transcription::Queued));
        jobs.failed.insert("b".into(), "bad".into());
        assert_eq!(
            jobs.transcription("b"),
            Some(Transcription::Failed("bad".into()))
        );
        assert_eq!(jobs.transcription("c"), None);
    }

    #[test]
    fn scanning_finds_installed_models() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan_models(dir.path()).is_empty());
        std::fs::write(VAD_MODEL.path(dir.path()), b"x").unwrap();
        std::fs::write(MODELS[1].path(dir.path()), b"x").unwrap();
        let found = scan_models(dir.path());
        assert!(found.contains(VAD_MODEL.id) && found.contains(MODELS[1].id));
        assert_eq!(found.len(), 2);
    }
}
