//! WASAPI capture on Windows: Teams' process tree through application loopback, the whole
//! system through classic loopback, and the microphone.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::time::Duration;

use sysinfo::{ProcessRefreshKind, RefreshKind, System};
use wasapi::{
    AudioCaptureClient, AudioClient, Device, DeviceEnumerator, Direction, Handle, Role, SampleType,
    StreamMode, WasapiError, WaveFormat,
};

use crate::audio::{Block, SAMPLE_RATE};
use crate::recorder::Backend;
use crate::source::{Opened, Source};
use crate::{Error, InputDevice, RemoteMode, Result};

/// Executable names of the new Teams client and classic Teams.
const TEAMS_EXES: [&str; 2] = ["ms-teams.exe", "teams.exe"];

impl From<WasapiError> for Error {
    fn from(e: WasapiError) -> Self {
        Error::Device(e.to_string())
    }
}

/// The real devices.
pub(crate) fn backend(mode: RemoteMode, mic_device: Option<String>) -> Backend {
    Backend {
        mic: Box::new(move || open_mic(mic_device.as_deref())),
        remote: Box::new(move || open_remote(mode)),
    }
}

/// Microphones, default communications device first.
pub(crate) fn input_devices() -> Result<Vec<InputDevice>> {
    com_init();
    let enumerator = DeviceEnumerator::new()?;
    let default_id = enumerator
        .get_default_device_for_role(&Direction::Capture, &Role::Communications)
        .and_then(|d| d.get_id())
        .ok();
    let mut devices = Vec::new();
    for device in &enumerator.get_device_collection(&Direction::Capture)? {
        let device = device?;
        devices.push(InputDevice {
            id: device.get_id()?,
            name: device.get_friendlyname()?,
        });
    }
    if let Some(id) = default_id
        && let Some(i) = devices.iter().position(|d| d.id == id)
    {
        let d = devices.remove(i);
        devices.insert(0, d);
    }
    Ok(devices)
}

/// The root process of the running Teams client: the Teams process whose parent is not
/// Teams. Capturing its tree includes the WebView2 children that render call audio.
pub(crate) fn teams_process() -> Option<u32> {
    let system = System::new_with_specifics(
        RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()),
    );
    let is_teams = |name: &OsStr| {
        let name = name.to_string_lossy().to_ascii_lowercase();
        TEAMS_EXES.contains(&name.as_str())
    };
    let processes = system.processes();
    processes
        .values()
        .filter(|p| is_teams(p.name()))
        .find(|p| {
            p.parent()
                .and_then(|pid| processes.get(&pid))
                .is_none_or(|parent| !is_teams(parent.name()))
        })
        .map(|p| p.pid().as_u32())
}

fn com_init() {
    // Already-initialized threads return S_FALSE or RPC_E_CHANGED_MODE; both are fine
    // for the shared-mode calls made here.
    let _ = wasapi::initialize_mta();
}

fn open_mic(id: Option<&str>) -> Result<Opened> {
    com_init();
    let enumerator = DeviceEnumerator::new()?;
    let chosen = match id {
        Some(id) => match enumerator.get_device(id) {
            Ok(d) => Some(d),
            Err(e) => {
                tracing::warn!(error = %e, "saved microphone not found; using the default");
                None
            }
        },
        None => None,
    };
    let fallback = id.is_some() && chosen.is_none();
    let device = match chosen {
        Some(d) => d,
        None => {
            enumerator.get_default_device_for_role(&Direction::Capture, &Role::Communications)?
        }
    };
    let name = device.get_friendlyname()?;
    let source = WasapiSource::from_device(&device, Direction::Capture, 1)?;
    Ok(Opened {
        source: Box::new(source),
        fallback: fallback.then(|| format!("The chosen microphone is missing; recording {name}.")),
        description: name,
    })
}

fn open_remote(mode: RemoteMode) -> Result<Opened> {
    com_init();
    if mode == RemoteMode::WholeSystem {
        return open_system_loopback(None);
    }
    let Some(pid) = teams_process() else {
        return open_system_loopback(Some(
            "Teams not detected. Capturing whole-system audio instead.".into(),
        ));
    };
    match WasapiSource::process_loopback(pid) {
        Ok(source) => Ok(Opened {
            source: Box::new(source),
            description: format!("Teams (process {pid})"),
            fallback: None,
        }),
        Err(e) => {
            tracing::warn!(error = %e, pid, "process loopback failed");
            open_system_loopback(Some(format!(
                "Could not capture Teams on its own ({e}). Capturing whole-system audio instead."
            )))
        }
    }
}

fn open_system_loopback(fallback: Option<String>) -> Result<Opened> {
    let enumerator = DeviceEnumerator::new()?;
    let device = enumerator.get_default_device(&Direction::Render)?;
    let name = device.get_friendlyname()?;
    let source = WasapiSource::from_device(&device, Direction::Capture, 2)?;
    Ok(Opened {
        source: Box::new(source),
        description: format!("whole system ({name})"),
        fallback,
    })
}

/// An event-driven shared-mode capture stream delivering 48 kHz float frames.
struct WasapiSource {
    // Kept alive for the stream's lifetime; stopping happens on drop.
    client: AudioClient,
    capture: AudioCaptureClient,
    event: Handle,
    queue: VecDeque<u8>,
    channels: u16,
}

impl WasapiSource {
    fn format(channels: u16) -> WaveFormat {
        WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            SAMPLE_RATE as usize,
            usize::from(channels),
            None,
        )
    }

    fn mode() -> StreamMode {
        StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: 0,
        }
    }

    /// Capture from a capture device, or loopback from a render device.
    fn from_device(device: &Device, direction: Direction, channels: u16) -> Result<Self> {
        let mut client = device.get_iaudioclient()?;
        client.initialize_client(&Self::format(channels), &direction, &Self::mode())?;
        Self::start(client, channels)
    }

    /// Loopback of one process tree (Windows 10 20348 / Windows 11 and later).
    fn process_loopback(pid: u32) -> Result<Self> {
        let mut client = AudioClient::new_application_loopback_client(pid, true)?;
        client.initialize_client(&Self::format(2), &Direction::Capture, &Self::mode())?;
        Self::start(client, 2)
    }

    fn start(client: AudioClient, channels: u16) -> Result<Self> {
        let event = client.set_get_eventhandle()?;
        let capture = client.get_audiocaptureclient()?;
        client.start_stream()?;
        Ok(Self {
            client,
            capture,
            event,
            queue: VecDeque::new(),
            channels,
        })
    }
}

impl Source for WasapiSource {
    fn read(&mut self, timeout: Duration) -> Result<Option<Block>> {
        match self.event.wait_for_event(timeout.as_millis() as u32) {
            Ok(()) => {}
            Err(WasapiError::EventTimeout) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        while self.capture.get_next_packet_size()?.unwrap_or(0) > 0 {
            let before = self.queue.len();
            let info = self.capture.read_from_device_to_deque(&mut self.queue)?;
            if info.flags.silent {
                for b in self.queue.range_mut(before..) {
                    *b = 0;
                }
            }
        }
        if self.queue.is_empty() {
            return Ok(None);
        }
        let usable = self.queue.len() - self.queue.len() % 4;
        let bytes: Vec<u8> = self.queue.drain(..usable).collect();
        let samples = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        Ok(Some(Block {
            samples,
            channels: self.channels,
            rate: SAMPLE_RATE,
        }))
    }
}

impl Drop for WasapiSource {
    fn drop(&mut self) {
        if let Err(e) = self.client.stop_stream() {
            tracing::debug!(error = %e, "stopping a capture stream");
        }
    }
}
