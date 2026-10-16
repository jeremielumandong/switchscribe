//! Core Audio capture on macOS: Teams' processes through a process tap, the whole system
//! through a global tap (both need macOS 14.2 or later), and the microphone through cpal.
//!
//! A tap is read through a private aggregate device that holds only that tap; an IO block
//! on Core Audio's real-time thread copies each buffer into a channel the capture thread
//! reads. The first tap asks for the "System Audio Recording" permission
//! (`NSAudioCaptureUsageDescription` in the bundle's Info.plist), the microphone for the
//! microphone permission (`NSMicrophoneUsageDescription`).

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use cpal::{FromSample, SampleFormat, SizedSample};
use objc2::rc::autoreleasepool;
use sysinfo::{ProcessRefreshKind, RefreshKind, System};

use crate::audio::Block;
use crate::recorder::Backend;
use crate::source::{Opened, Source};
use crate::{Error, InputDevice, RemoteMode, Result};

/// Blocks queued between an audio callback and the capture thread (about two seconds at
/// 10 ms buffers). A full queue drops blocks rather than stall Core Audio's thread.
const QUEUE: usize = 200;

/// Prefix of the private aggregate devices that read taps; kept out of the mic list.
const TAP_DEVICE_UID: &str = "app.switchscribe.tap.";

/// What an audio callback hands the capture thread.
enum Msg {
    Audio(Block),
    Failed(String),
}

fn receive(rx: &Receiver<Msg>, timeout: Duration) -> Result<Option<Block>> {
    match rx.recv_timeout(timeout) {
        Ok(Msg::Audio(block)) => Ok(Some(block)),
        Ok(Msg::Failed(e)) => Err(Error::Device(e)),
        Err(RecvTimeoutError::Timeout) => Ok(None),
        Err(RecvTimeoutError::Disconnected) => {
            Err(Error::Device("the audio stream stopped".into()))
        }
    }
}

fn dev_err(e: cpal::Error) -> Error {
    Error::Device(e.to_string())
}

/// The real devices.
pub(crate) fn backend(mode: RemoteMode, mic_device: Option<String>) -> Backend {
    // Each in its own autorelease pool, drained when the source is open: capture threads
    // have no pool of their own, so anything Core Audio or cpal autoreleases would otherwise
    // wait for the thread to exit, which is when the recording stops.
    Backend {
        mic: Box::new(move || autoreleasepool(|_| open_mic(mic_device.as_deref()))),
        remote: Box::new(move || autoreleasepool(|_| open_remote(mode))),
    }
}

/// Microphones, the system's default input first.
pub(crate) fn input_devices() -> Result<Vec<InputDevice>> {
    let host = cpal::default_host();
    let default_id = host
        .default_input_device()
        .and_then(|d| d.id().ok())
        .map(|id| id.to_string());
    let mut devices = Vec::new();
    for device in host.input_devices().map_err(dev_err)? {
        let Ok(id) = device.id() else { continue };
        let id = id.to_string();
        if id.contains(TAP_DEVICE_UID) {
            continue;
        }
        devices.push(InputDevice {
            name: device_name(&device),
            id,
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

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "Microphone".into())
}

fn open_mic(id: Option<&str>) -> Result<Opened> {
    let host = cpal::default_host();
    let chosen = id.and_then(|id| {
        let found = host
            .input_devices()
            .ok()?
            .find(|d| d.id().is_ok_and(|d| d.to_string() == id));
        if found.is_none() {
            tracing::warn!("saved microphone not found; using the default");
        }
        found
    });
    let fallback = id.is_some() && chosen.is_none();
    let device = match chosen {
        Some(d) => d,
        None => host
            .default_input_device()
            .ok_or_else(|| Error::Device("No microphone found.".into()))?,
    };
    let name = device_name(&device);
    let source = CpalSource::open(&device)?;
    Ok(Opened {
        source: Box::new(source),
        fallback: fallback.then(|| format!("The chosen microphone is missing; recording {name}.")),
        description: name,
    })
}

/// A cpal input stream.
struct CpalSource {
    // Kept alive for the stream's lifetime; dropping it stops capture.
    _stream: cpal::Stream,
    rx: Receiver<Msg>,
}

impl CpalSource {
    fn open(device: &cpal::Device) -> Result<Self> {
        let supported = device.default_input_config().map_err(dev_err)?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let stream = match format {
            SampleFormat::F32 => build::<f32>(device, &config, tx),
            SampleFormat::I16 => build::<i16>(device, &config, tx),
            SampleFormat::I32 => build::<i32>(device, &config, tx),
            SampleFormat::U16 => build::<u16>(device, &config, tx),
            other => return Err(Error::Device(format!("unsupported sample format {other}"))),
        }
        .map_err(dev_err)?;
        stream.play().map_err(dev_err)?;
        Ok(Self {
            _stream: stream,
            rx,
        })
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: SyncSender<Msg>,
) -> std::result::Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels;
    let rate = config.sample_rate;
    let err_tx = tx.clone();
    device.build_input_stream(
        *config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let samples = data.iter().map(|s| s.to_sample::<f32>()).collect();
            // Never block the audio thread; a full queue means the reader stalled.
            let _ = tx.try_send(Msg::Audio(Block {
                samples,
                channels,
                rate,
            }));
        },
        move |e| match e.kind() {
            // Rerouted to the new default device; capture goes on.
            cpal::ErrorKind::DeviceChanged => tracing::info!("microphone: {e}"),
            _ => {
                let _ = err_tx.try_send(Msg::Failed(e.to_string()));
            }
        },
        None,
    )
}

impl Source for CpalSource {
    fn read(&mut self, timeout: Duration) -> Result<Option<Block>> {
        receive(&self.rx, timeout)
    }
}

fn open_remote(mode: RemoteMode) -> Result<Opened> {
    if let Some(version) = System::os_version()
        && !supports_taps(&version)
    {
        return Err(Error::Device(format!(
            "it needs macOS 14.2 or later; this Mac runs {version}"
        )));
    }
    if mode == RemoteMode::WholeSystem {
        return open_system_tap(None);
    }
    let teams = match tap::processes() {
        Ok(processes) => teams_objects(&processes),
        Err(e) => {
            tracing::warn!(error = %e, "could not list audio processes");
            Vec::new()
        }
    };
    if teams.is_empty() {
        return open_system_tap(Some(
            "Teams not detected. Capturing whole-system audio instead.".into(),
        ));
    }
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    match tap::Tap::open(tap::Scope::Processes(&teams), tx) {
        Ok(tap) => Ok(Opened {
            source: Box::new(TapSource { _tap: tap, rx }),
            description: format!("Teams ({} audio processes)", teams.len()),
            fallback: None,
        }),
        Err(e) => {
            tracing::warn!(error = %e, "Teams process tap failed");
            open_system_tap(Some(format!(
                "Could not capture Teams on its own ({e}). Capturing whole-system audio instead."
            )))
        }
    }
}

fn open_system_tap(fallback: Option<String>) -> Result<Opened> {
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let tap = tap::Tap::open(tap::Scope::Everything, tx)?;
    Ok(Opened {
        source: Box::new(TapSource { _tap: tap, rx }),
        description: "whole system".into(),
        fallback,
    })
}

/// Reads a tap's samples.
struct TapSource {
    // Stopped and destroyed on drop.
    _tap: tap::Tap,
    rx: Receiver<Msg>,
}

impl Source for TapSource {
    fn read(&mut self, timeout: Duration) -> Result<Option<Block>> {
        receive(&self.rx, timeout)
    }
}

/// Whether a `major.minor[.patch]` macOS version has process taps (14.2 and later).
fn supports_taps(version: &str) -> bool {
    let mut parts = version.trim().split('.').map(|p| p.parse::<u32>().ok());
    let major = parts.next().flatten();
    let minor = parts.next().flatten().unwrap_or(0);
    match major {
        Some(major) => (major, minor) >= (14, 2),
        // Unparseable: try, and let Core Audio say no.
        None => true,
    }
}

/// Process names of Teams and its helpers ("MSTeams", "Microsoft Teams", "Microsoft Teams
/// WebView Helper (Renderer)", "Microsoft Teams Modulehost"...).
fn is_teams_name(name: &str) -> bool {
    name == "MSTeams" || name.starts_with("Microsoft Teams")
}

/// Teams' processes and their descendants, by pid. The helpers that render and play call
/// audio are children of the Teams app.
fn teams_tree() -> HashSet<u32> {
    let system = System::new_with_specifics(
        RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()),
    );
    let processes = system.processes();
    let mut tree: HashSet<u32> = processes
        .values()
        .filter(|p| is_teams_name(&p.name().to_string_lossy()))
        .map(|p| p.pid().as_u32())
        .collect();
    // Grow the set to a fixed point: children of anything in it.
    loop {
        let before = tree.len();
        for p in processes.values() {
            if p.parent()
                .is_some_and(|parent| tree.contains(&parent.as_u32()))
            {
                tree.insert(p.pid().as_u32());
            }
        }
        if tree.len() == before {
            return tree;
        }
    }
}

fn teams_processes(processes: &[tap::Process]) -> Vec<&tap::Process> {
    let tree = teams_tree();
    processes
        .iter()
        .filter(|p| u32::try_from(p.pid).is_ok_and(|pid| tree.contains(&pid)))
        .collect()
}

/// The Core Audio process objects of Teams' tree.
fn teams_objects(processes: &[tap::Process]) -> Vec<tap::AudioObjectID> {
    teams_processes(processes)
        .into_iter()
        .map(|p| p.object)
        .collect()
}

/// A call is Teams holding the microphone. Core Audio reports that per process (macOS
/// 14.2 and later); Teams keeps its input running while muted, so it lasts the call.
pub(crate) struct CallProbe;

impl crate::detect::Probe for CallProbe {
    fn probe(&mut self) -> Option<crate::detect::TeamsCall> {
        // The detection thread never exits, so drain each look's autoreleased objects.
        autoreleasepool(|_| probe_once())
    }
}

fn probe_once() -> Option<crate::detect::TeamsCall> {
    let processes = match tap::processes() {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(error = %e, "could not list audio processes");
            return None;
        }
    };
    let teams = teams_processes(&processes);
    let calling = teams
        .into_iter()
        .find(|p| tap::is_running_input(p.object))?;
    Some(crate::detect::TeamsCall {
        pid: u32::try_from(calling.pid).unwrap_or(0),
        // Other apps' window titles need the Screen Recording permission on macOS;
        // the calendar names the meeting instead.
        title: None,
    })
}

/// The Core Audio calls: process objects, taps and the aggregate device that reads one.
#[allow(unsafe_code)]
mod tap {
    use std::ffi::{CStr, c_void};
    use std::mem::MaybeUninit;
    use std::ptr::{NonNull, null};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::SyncSender;

    use block2::RcBlock;
    use objc2::AnyThread as _;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_core_audio::{
        AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
        AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
        AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
        AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
        AudioObjectPropertyAddress, AudioObjectPropertySelector, CATapDescription,
        CATapMuteBehavior, kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceNameKey,
        kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
        kAudioAggregateDeviceUIDKey, kAudioHardwarePropertyProcessObjectList,
        kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
        kAudioProcessPropertyIsRunningInput, kAudioProcessPropertyPID,
        kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyFormat,
    };
    use objc2_core_audio_types::{
        AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
        kAudioFormatFlagIsFloat, kAudioFormatLinearPCM,
    };
    use objc2_core_foundation::CFDictionary;
    use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

    use super::{Msg, TAP_DEVICE_UID};
    use crate::audio::Block;
    use crate::{Error, Result};

    pub(crate) use objc2_core_audio::AudioObjectID;

    const SYSTEM: AudioObjectID = kAudioObjectSystemObject as AudioObjectID;

    static INSTANCE: AtomicU32 = AtomicU32::new(0);

    /// A process Core Audio knows about.
    #[derive(Debug)]
    pub(crate) struct Process {
        /// Its process object.
        pub object: AudioObjectID,
        /// Its pid.
        pub pid: i32,
    }

    fn address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    fn check(status: i32, what: &str) -> Result<()> {
        if status == 0 {
            return Ok(());
        }
        // Core Audio statuses are usually four printable characters ('!obj', 'who?').
        let bytes = status.to_be_bytes();
        let code = if bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            format!("'{}'", String::from_utf8_lossy(&bytes))
        } else {
            status.to_string()
        };
        Err(Error::Device(format!(
            "{what} failed (Core Audio error {code})"
        )))
    }

    /// A property of plain-data type `T`.
    ///
    /// # Safety
    /// `T` must be the type Core Audio stores for `selector` on `object`.
    unsafe fn get<T: Copy>(
        object: AudioObjectID,
        selector: AudioObjectPropertySelector,
        qualifier: Option<&u32>,
    ) -> Result<T> {
        let addr = address(selector);
        let mut value = MaybeUninit::<T>::uninit();
        let mut size = size_of::<T>() as u32;
        let (q_size, q_ptr) = match qualifier {
            Some(q) => (size_of::<u32>() as u32, (q as *const u32).cast::<c_void>()),
            None => (0, null()),
        };
        // SAFETY: `value` has room for a `T`, which the caller vouches is the property's type.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&addr),
                q_size,
                q_ptr,
                NonNull::from(&mut size),
                NonNull::new_unchecked(value.as_mut_ptr().cast()),
            )
        };
        check(status, "reading an audio property")?;
        if size as usize != size_of::<T>() {
            return Err(Error::Device(
                "an audio property had an unexpected size".into(),
            ));
        }
        // SAFETY: Core Audio filled all `size_of::<T>()` bytes.
        Ok(unsafe { value.assume_init() })
    }

    /// A list of audio object ids.
    fn get_ids(object: AudioObjectID, selector: AudioObjectPropertySelector) -> Result<Vec<u32>> {
        let addr = address(selector);
        let mut size = 0u32;
        // SAFETY: plain size query.
        let status = unsafe {
            AudioObjectGetPropertyDataSize(
                object,
                NonNull::from(&addr),
                0,
                null(),
                NonNull::from(&mut size),
            )
        };
        check(status, "listing audio objects")?;
        let mut ids = vec![0u32; size as usize / size_of::<u32>()];
        if ids.is_empty() {
            return Ok(ids);
        }
        let mut size = (ids.len() * size_of::<u32>()) as u32;
        // SAFETY: `ids` holds `size` bytes.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&addr),
                0,
                null(),
                NonNull::from(&mut size),
                NonNull::new_unchecked(ids.as_mut_ptr().cast()),
            )
        };
        check(status, "listing audio objects")?;
        ids.truncate(size as usize / size_of::<u32>());
        Ok(ids)
    }

    /// Every process with a connection to the audio server (macOS 14.2 and later).
    pub(crate) fn processes() -> Result<Vec<Process>> {
        let mut out = Vec::new();
        for object in get_ids(SYSTEM, kAudioHardwarePropertyProcessObjectList)? {
            // SAFETY: a process's pid is a `pid_t`.
            let Ok(pid) = (unsafe { get::<i32>(object, kAudioProcessPropertyPID, None) }) else {
                continue;
            };
            out.push(Process { object, pid });
        }
        Ok(out)
    }

    /// Whether a process is capturing audio (holding a microphone).
    pub(crate) fn is_running_input(object: AudioObjectID) -> bool {
        // SAFETY: the property is a `UInt32` flag.
        unsafe { get::<u32>(object, kAudioProcessPropertyIsRunningInput, None) }
            .is_ok_and(|v| v != 0)
    }

    /// This app's own process object, if it has one, to keep its playback out of a global tap.
    fn own_object() -> Option<AudioObjectID> {
        let pid = std::process::id();
        // SAFETY: the translation takes a pid qualifier and returns an `AudioObjectID`.
        unsafe {
            get::<AudioObjectID>(
                SYSTEM,
                kAudioHardwarePropertyTranslatePIDToProcessObject,
                Some(&pid),
            )
        }
        .ok()
        .filter(|&o| o != 0)
    }

    /// What a tap records.
    pub(crate) enum Scope<'a> {
        /// These process objects, mixed to stereo.
        Processes(&'a [AudioObjectID]),
        /// Everything playing, except this app.
        Everything,
    }

    type IoBlock = RcBlock<
        dyn Fn(
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
        ),
    >;

    /// A running tap: the tap, the private aggregate device reading it and its IO block.
    pub(crate) struct Tap {
        tap: AudioObjectID,
        device: AudioObjectID,
        proc_id: AudioDeviceIOProcID,
        // The HAL calls this until `proc_id` is destroyed.
        _block: Option<IoBlock>,
    }

    fn ns(key: &CStr) -> Retained<NSString> {
        NSString::from_str(&key.to_string_lossy())
    }

    impl Tap {
        /// Create a tap for `scope` and start sending its audio to `tx`.
        pub(crate) fn open(scope: Scope<'_>, tx: SyncSender<Msg>) -> Result<Self> {
            let instance = INSTANCE.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();

            // 1. The tap.
            let desc = match scope {
                Scope::Processes(objects) => {
                    let numbers: Vec<Retained<NSNumber>> =
                        objects.iter().map(|&o| NSNumber::new_u32(o)).collect();
                    let list = NSArray::from_retained_slice(&numbers);
                    // SAFETY: the array holds process object ids, as the initializer wants.
                    unsafe {
                        CATapDescription::initStereoMixdownOfProcesses(
                            CATapDescription::alloc(),
                            &list,
                        )
                    }
                }
                Scope::Everything => {
                    let numbers: Vec<Retained<NSNumber>> =
                        own_object().into_iter().map(NSNumber::new_u32).collect();
                    let list = NSArray::from_retained_slice(&numbers);
                    // SAFETY: as above, the processes to leave out.
                    unsafe {
                        CATapDescription::initStereoGlobalTapButExcludeProcesses(
                            CATapDescription::alloc(),
                            &list,
                        )
                    }
                }
            };
            // SAFETY: plain setters on a description this function owns.
            unsafe {
                desc.setName(&NSString::from_str(&format!(
                    "switchscribe {pid}.{instance}"
                )));
                desc.setPrivate(true);
                // Teams stays audible: the tap copies, it does not take the audio away.
                desc.setMuteBehavior(CATapMuteBehavior::Unmuted);
            }
            let mut tap: AudioObjectID = 0;
            // SAFETY: `desc` is a valid description and `tap` receives the new id.
            let status = unsafe { AudioHardwareCreateProcessTap(Some(&desc), &raw mut tap) };
            check(status, "creating the audio tap")?;
            let mut this = Self {
                tap,
                device: 0,
                proc_id: None,
                _block: None,
            };

            // SAFETY: a tap's format is an `AudioStreamBasicDescription`.
            let format =
                unsafe { get::<AudioStreamBasicDescription>(tap, kAudioTapPropertyFormat, None) }?;
            if format.mFormatID != kAudioFormatLinearPCM
                || format.mFormatFlags & kAudioFormatFlagIsFloat == 0
                || format.mBitsPerChannel != 32
            {
                return Err(Error::Device(format!(
                    "the tap delivers an unsupported format ({} bits, flags {:#x})",
                    format.mBitsPerChannel, format.mFormatFlags
                )));
            }
            let rate = format.mSampleRate.round() as u32;

            // 2. A private aggregate device with only the tap in it, started with it.
            // SAFETY: the tap's UUID was assigned when the description was made.
            let tap_uid = unsafe { desc.UUID() }.UUIDString();
            let yes = NSNumber::new_bool(true);
            let sub_tap = NSDictionary::<NSString, AnyObject>::from_slices(
                &[
                    &*ns(kAudioSubTapUIDKey),
                    &*ns(kAudioSubTapDriftCompensationKey),
                ],
                &[tap_uid.as_ref(), yes.as_ref()],
            );
            let taps = NSArray::from_retained_slice(&[sub_tap]);
            let name = NSString::from_str("switchscribe tap");
            let uid = NSString::from_str(&format!("{TAP_DEVICE_UID}{pid}.{instance}"));
            let description = NSDictionary::<NSString, AnyObject>::from_slices(
                &[
                    &*ns(kAudioAggregateDeviceNameKey),
                    &*ns(kAudioAggregateDeviceUIDKey),
                    &*ns(kAudioAggregateDeviceTapListKey),
                    &*ns(kAudioAggregateDeviceTapAutoStartKey),
                    &*ns(kAudioAggregateDeviceIsPrivateKey),
                ],
                &[
                    name.as_ref(),
                    uid.as_ref(),
                    taps.as_ref(),
                    yes.as_ref(),
                    yes.as_ref(),
                ],
            );
            // SAFETY: NSDictionary is toll-free bridged to CFDictionary.
            let cf = unsafe { &*Retained::as_ptr(&description).cast::<CFDictionary>() };
            let mut device: AudioObjectID = 0;
            // SAFETY: `cf` describes an aggregate device; `device` receives its id.
            let status =
                unsafe { AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut device)) };
            check(status, "creating the tap's aggregate device")?;
            this.device = device;

            // 3. The IO block, on Core Audio's real-time thread: copy and hand over.
            let block: IoBlock = RcBlock::new(
                move |_now: NonNull<AudioTimeStamp>,
                      input: NonNull<AudioBufferList>,
                      _input_time: NonNull<AudioTimeStamp>,
                      _output: NonNull<AudioBufferList>,
                      _output_time: NonNull<AudioTimeStamp>| {
                    // SAFETY: Core Audio passes a valid buffer list for this cycle.
                    let block = unsafe { read_buffers(input, rate) };
                    if let Some(block) = block {
                        let _ = tx.try_send(Msg::Audio(block));
                    }
                },
            );
            let mut proc_id: AudioDeviceIOProcID = None;
            // SAFETY: `device` is the aggregate just made; the block outlives the proc id
            // (both live in `this`, and drop destroys the proc id first).
            let status = unsafe {
                AudioDeviceCreateIOProcIDWithBlock(
                    NonNull::from(&mut proc_id),
                    device,
                    None,
                    RcBlock::as_ptr(&block),
                )
            };
            check(status, "registering the tap's reader")?;
            this.proc_id = proc_id;
            this._block = Some(block);
            // SAFETY: a registered proc id on its device.
            let status = unsafe { AudioDeviceStart(device, proc_id) };
            check(status, "starting the tap")?;
            tracing::info!(
                rate,
                channels = format.mChannelsPerFrame,
                "audio tap started"
            );
            Ok(this)
        }
    }

    /// Copy one cycle's input into an interleaved block.
    ///
    /// # Safety
    /// `list` must be a valid buffer list of 32-bit float samples.
    unsafe fn read_buffers(list: NonNull<AudioBufferList>, rate: u32) -> Option<Block> {
        // SAFETY: per the caller; the list has `mNumberBuffers` buffers inline.
        let buffers: &[AudioBuffer] = unsafe {
            let list = list.as_ref();
            std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize)
        };
        let floats = |b: &AudioBuffer| -> &[f32] {
            if b.mData.is_null() {
                return &[];
            }
            // SAFETY: the buffer holds `mDataByteSize` bytes of f32 samples.
            unsafe {
                std::slice::from_raw_parts(
                    b.mData.cast::<f32>(),
                    b.mDataByteSize as usize / size_of::<f32>(),
                )
            }
        };
        match buffers {
            [] => None,
            [one] => {
                let samples = floats(one).to_vec();
                (!samples.is_empty()).then(|| Block {
                    samples,
                    channels: one.mNumberChannels.max(1) as u16,
                    rate,
                })
            }
            planes => {
                // Non-interleaved: one channel per buffer. Interleave them.
                let planes: Vec<&[f32]> = planes.iter().map(floats).collect();
                let frames = planes.iter().map(|p| p.len()).min().unwrap_or(0);
                if frames == 0 {
                    return None;
                }
                let mut samples = Vec::with_capacity(frames * planes.len());
                for i in 0..frames {
                    samples.extend(planes.iter().map(|p| p[i]));
                }
                Some(Block {
                    samples,
                    channels: planes.len() as u16,
                    rate,
                })
            }
        }
    }

    impl Drop for Tap {
        fn drop(&mut self) {
            // SAFETY: each id is destroyed once, in reverse order of creation; statuses are
            // ignored since nothing can be done about them here.
            unsafe {
                if self.proc_id.is_some() {
                    AudioDeviceStop(self.device, self.proc_id);
                    AudioDeviceDestroyIOProcID(self.device, self.proc_id);
                }
                if self.device != 0 {
                    AudioHardwareDestroyAggregateDevice(self.device);
                }
                AudioHardwareDestroyProcessTap(self.tap);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taps_need_macos_14_2() {
        assert!(supports_taps("14.2"));
        assert!(supports_taps("14.4.1"));
        assert!(supports_taps("15.0"));
        assert!(supports_taps("26.1"));
        assert!(!supports_taps("14.1.2"));
        assert!(!supports_taps("13.6"));
        assert!(supports_taps("unknown"));
    }

    #[test]
    fn recognizes_teams() {
        assert!(is_teams_name("MSTeams"));
        assert!(is_teams_name("Microsoft Teams WebView Helper (Renderer)"));
        assert!(!is_teams_name("Microsoft Edge"));
    }

    /// Lists audio processes and microphones; needs no permission, so it runs in CI.
    #[test]
    fn lists_devices_and_processes() {
        let _ = input_devices();
        if System::os_version().is_some_and(|v| supports_taps(&v)) {
            assert!(tap::processes().is_ok());
        }
    }
}
