//! Windows process-tree loopback. COM objects and GetBuffer/ReleaseBuffer remain on one owner thread.
use crate::cpal::{DeviceDirection, NativeAudioError};
use crate::ports::{self, CaptureFlags, CaptureIngress, FrameReader, PortTelemetry};
use audiokit::resample_format::ResampleBlockFormat as PcmFormat;
use audiokit::{AudioError, AudioFormat, AudioResult};
use std::{
    mem::size_of,
    pin::Pin,
    slice,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
const MIN_PROCESS_LOOPBACK_BUILD: u32 = 20_348;
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(7);
const CAPTURE_WAIT_MS: u32 = 100;

/// Process loopback port with exclusive raw-frame reader and observable terminal state.
pub struct WasapiProcessCapture {
    reader: FrameReader,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    telemetry: Arc<PortTelemetry>,
    failure: Arc<Mutex<Option<NativeAudioError>>>,
}
impl WasapiProcessCapture {
    /// Opens a non-self PID, retaining its process object to prevent PID-reuse retargeting.
    /// The host owns system-audio consent. Both inclusion and exclusion tree modes are explicit.
    pub fn open(
        process_id: u32,
        include_tree: bool,
        format: AudioFormat,
        queue_ms: u16,
    ) -> Result<Self, NativeAudioError> {
        ensure_supported_windows_build()?;
        let shape = PcmFormat::new(format.sample_rate_hz(), format.channels(), 20)
            .map_err(|e| NativeAudioError::UnsupportedConfig(e.to_string()))?;
        validate_request(process_id, shape)?;
        if !(20..=500).contains(&queue_ms) {
            return Err(NativeAudioError::UnsupportedConfig(
                "queue_ms must be 20..=500".into(),
            ));
        }
        let capacity =
            (u64::from(format.sample_rate_hz()) * u64::from(queue_ms)).div_ceil(1000) as usize;
        let (mut ingress, reader) = ports::capture_pair(format, capacity)
            .map_err(|e| NativeAudioError::UnsupportedConfig(e.to_string()))?;
        let telemetry = ingress.telemetry();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_telemetry = Arc::clone(&telemetry);
        let failure = Arc::new(Mutex::new(None));
        let worker_failure = Arc::clone(&failure);
        let (ready, started) = std_mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("audiokit-wasapi-process".into())
            .spawn(move || {
                // SAFETY: this worker pairs successful COM initialization with same-thread teardown.
                if let Err(e) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
                    let _ = ready.send(Err(NativeAudioError::Backend(format!(
                        "initialize COM: {e}"
                    ))));
                    return;
                }
                let _com = ComRuntime;
                let state = match start_capture(process_id, include_tree, shape) {
                    Ok(state) => state,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                if ready.send(Ok(())).is_err() {
                    return;
                }
                if let Err(error) = capture_packets(&state, shape, &mut ingress, &worker_stop) {
                    // This is a worker, not a device callback; retain the precise failure for the host.
                    if let Ok(mut slot) = worker_failure.lock() {
                        *slot = Some(error);
                    }
                    worker_telemetry.fail(4);
                }
                worker_telemetry.stop();
            })
            .map_err(|e| NativeAudioError::Backend(format!("spawn WASAPI worker: {e}")))?;
        match started.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                reader,
                stop,
                worker: Some(worker),
                telemetry,
                failure,
            }),
            result => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err(match result {
                    Ok(Err(e)) => e,
                    _ => NativeAudioError::Backend("WASAPI startup failed/timed out".into()),
                })
            }
        }
    }
    /// Returns configured native PCM shape, independent of media packetization.
    pub fn format(&self) -> AudioFormat {
        self.reader.format()
    }
    /// Reads one frame with device position, QPC-derived raw timestamp and native flags.
    pub fn read_device_frame(&mut self) -> Option<ports::DeviceFrame> {
        self.reader.pop()
    }
    /// Returns native queue depth in per-channel frames.
    pub fn queued_frames(&self) -> usize {
        self.reader.queued_frames()
    }
    /// Returns PCM-free health; raw device time is not claimed as mapped host time.
    pub fn telemetry(&self) -> Arc<PortTelemetry> {
        Arc::clone(&self.telemetry)
    }
    /// Takes the detailed worker failure once; terminal telemetry remains observable.
    pub fn take_failure(&self) -> Option<NativeAudioError> {
        self.failure.lock().ok().and_then(|mut error| error.take())
    }
    /// Requests stop and joins the COM owner after bounded event-wait wakeup.
    pub fn stop(&mut self) -> AudioResult<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| AudioError::Processing("WASAPI worker panicked".into()))?;
        }
        self.telemetry.stop();
        Ok(())
    }
}
impl Drop for WasapiProcessCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Media::Audio,
        System::{
            Com::{
                BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize,
                StructuredStorage::PROPVARIANT,
            },
            Threading::{CreateEventW, OpenProcess, PROCESS_SYNCHRONIZE, WaitForMultipleObjects},
            Variant::{VT_BLOB, VT_EMPTY},
        },
    },
    core::{Error as WindowsError, HRESULT, IUnknown, Interface, Ref},
};
fn validate_request(process_id: u32, format: PcmFormat) -> Result<(), NativeAudioError> {
    if process_id == 0 || process_id == std::process::id() {
        return Err(NativeAudioError::UnsupportedConfig(
            "process loopback requires a nonzero PID other than this voice client".to_owned(),
        ));
    }
    if !matches!(format.channels, 1 | 2)
        || !matches!(format.ptime_ms, 10 | 20 | 40 | 60)
        || format.sample_rate == 0
    {
        return Err(NativeAudioError::UnsupportedConfig(
            "process loopback supports mono/stereo Opus PCM packet formats".to_owned(),
        ));
    }
    Ok(())
}

fn ensure_supported_windows_build() -> Result<(), NativeAudioError> {
    #[repr(C)]
    struct RtlOsVersionInfoW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(version: *mut RtlOsVersionInfoW) -> i32;
    }

    let mut version = RtlOsVersionInfoW {
        size: size_of::<RtlOsVersionInfoW>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    // RtlGetVersion reports the actual kernel version, independent of an application manifest.
    let status = unsafe { RtlGetVersion(&mut version) };
    if status != 0 {
        return Err(NativeAudioError::Backend(format!(
            "query Windows version failed with NTSTATUS 0x{:08x}",
            status as u32
        )));
    }
    if version.major < 10 || (version.major == 10 && version.build < MIN_PROCESS_LOOPBACK_BUILD) {
        return Err(NativeAudioError::UnsupportedPlatform(format!(
            "WASAPI process loopback requires Windows 10 build {MIN_PROCESS_LOOPBACK_BUILD} or newer"
        )));
    }
    Ok(())
}

struct ComRuntime;

impl Drop for ComRuntime {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

struct StartedAudioClient(Audio::IAudioClient);

impl Drop for StartedAudioClient {
    fn drop(&mut self) {
        let _ = unsafe { self.0.Stop() };
    }
}

struct CaptureState {
    process_id: u32,
    _client: StartedAudioClient,
    _activation: ProcessLoopbackActivation,
    capture: Audio::IAudioCaptureClient,
    _event: HandleGuard,
    _process: HandleGuard,
}

fn start_capture(
    process_id: u32,
    include_process_tree: bool,
    format: PcmFormat,
) -> Result<CaptureState, NativeAudioError> {
    let process =
        unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, process_id) }.map_err(|error| {
            NativeAudioError::DeviceUnavailable {
                direction: DeviceDirection::Input,
                detail: format!("target process {process_id} is unavailable: {error}"),
            }
        })?;
    let process = HandleGuard(process);
    match unsafe { WaitForMultipleObjects(&[process.0], false, 0) } {
        WAIT_OBJECT_0 => return Err(process_unavailable(process_id)),
        WAIT_TIMEOUT => {}
        _ => {
            return Err(NativeAudioError::Backend(format!(
                "verify target process {process_id}: {}",
                WindowsError::from_thread()
            )));
        }
    }

    // Keep the process object open for the entire capture so a recycled PID cannot retarget it.
    let activation = ProcessLoopbackActivation::activate(process_id, include_process_tree)?;
    let client = activation.client().clone();
    if unsafe { WaitForMultipleObjects(&[process.0], false, 0) } == WAIT_OBJECT_0 {
        return Err(process_unavailable(process_id));
    }
    let event = HandleGuard(
        unsafe { CreateEventW(None, false, false, None) }
            .map_err(|error| NativeAudioError::Backend(format!("create WASAPI event: {error}")))?,
    );
    let wave_format = pcm16_wave_format(format);
    let stream_flags = Audio::AUDCLNT_STREAMFLAGS_LOOPBACK
        | Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | Audio::AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | Audio::AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    let buffer_duration_hns = i64::from(format.ptime_ms) * 20_000;

    unsafe {
        client
            .Initialize(
                Audio::AUDCLNT_SHAREMODE_SHARED,
                stream_flags,
                buffer_duration_hns,
                0,
                &wave_format,
                None,
            )
            .map_err(|error| {
                NativeAudioError::UnsupportedConfig(format!(
                    "initialize WASAPI process loopback format: {error}"
                ))
            })?;
        client
            .SetEventHandle(event.0)
            .map_err(|error| NativeAudioError::Backend(format!("set WASAPI event: {error}")))?;
    }
    let capture =
        unsafe { client.GetService::<Audio::IAudioCaptureClient>() }.map_err(|error| {
            NativeAudioError::Backend(format!("open WASAPI capture client: {error}"))
        })?;
    if unsafe { WaitForMultipleObjects(&[process.0], false, 0) } == WAIT_OBJECT_0 {
        return Err(process_unavailable(process_id));
    }
    unsafe { client.Start() }
        .map_err(|error| NativeAudioError::Backend(format!("start WASAPI capture: {error}")))?;
    if unsafe { WaitForMultipleObjects(&[process.0], false, 0) } == WAIT_OBJECT_0 {
        let _ = unsafe { client.Stop() };
        return Err(process_unavailable(process_id));
    }

    Ok(CaptureState {
        process_id,
        _client: StartedAudioClient(client),
        _activation: activation,
        capture,
        _event: event,
        _process: process,
    })
}

fn pcm16_wave_format(format: PcmFormat) -> Audio::WAVEFORMATEX {
    let channels = u16::from(format.channels);
    let block_align = channels * 2;
    Audio::WAVEFORMATEX {
        wFormatTag: Audio::WAVE_FORMAT_PCM as u16,
        nChannels: channels,
        nSamplesPerSec: format.sample_rate,
        nAvgBytesPerSec: format.sample_rate * u32::from(block_align),
        nBlockAlign: block_align,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// Same-thread COM activation owner, also usable by a transitional native host adapter.
/// Neither the owner nor its client should leave the COM thread that initialized capture.
pub struct ProcessLoopbackActivation {
    client: Audio::IAudioClient,
    _operation: Audio::IActivateAudioInterfaceAsyncOperation,
    _completion_handler: Audio::IActivateAudioInterfaceCompletionHandler,
    _activation_lifetime: Arc<ActivationLifetime>,
    _owner_thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl ProcessLoopbackActivation {
    /// Activates on the current COM-initialized owner thread and retains asynchronous arguments.
    /// Caller keeps the target process handle live and enforces its capture/permission policy.
    pub fn activate(process_id: u32, include_process_tree: bool) -> Result<Self, NativeAudioError> {
        if process_id == 0 || process_id == std::process::id() {
            return Err(NativeAudioError::UnsupportedConfig(
                "process loopback requires a non-self, nonzero PID".into(),
            ));
        }
        activate_process_loopback(process_id, include_process_tree)
    }
    /// Borrows the native client for same-thread Initialize/GetService/Start operations.
    pub fn client(&self) -> &Audio::IAudioClient {
        &self.client
    }
}

// Both pointees become immutable before sharing. The only pointer in the blob
// addresses its own pinned parameters; neither structure contains a COM object.
struct ActivationLifetime {
    blob: Pin<Box<PROPVARIANT>>,
    _parameters: Pin<Box<Audio::AUDIOCLIENT_ACTIVATION_PARAMS>>,
}
impl Drop for ActivationLifetime {
    fn drop(&mut self) {
        // PROPVARIANT normally frees VT_BLOB with CoTaskMemFree. This blob BORROWS
        // a Rust-owned pinned Box instead: disarm that foreign deallocator first.
        // SAFETY: the last Arc gives exclusive access; Windows has released its handler.
        unsafe {
            let value = &mut *self.blob.as_mut().get_mut().Anonymous.Anonymous;
            value.vt = VT_EMPTY;
            value.Anonymous.blob = BLOB::default();
        }
    }
}
// SAFETY: ownership keeps both immutable pinned allocations live across threads;
// Windows only reads them, and no public API exposes mutation or the raw pointer.
unsafe impl Send for ActivationLifetime {}
// SAFETY: all access after construction is read-only, including the native activation call.
unsafe impl Sync for ActivationLifetime {}

fn activation_lifetime(process_id: u32, include_process_tree: bool) -> Arc<ActivationLifetime> {
    // The async API retains raw pointers, so pin both structures through capture teardown.
    let mut parameters = Box::pin(Audio::AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: Audio::AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: Audio::AUDIOCLIENT_ACTIVATION_PARAMS_0::default(),
    });
    parameters
        .as_mut()
        .get_mut()
        .Anonymous
        .ProcessLoopbackParams = Audio::AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
        TargetProcessId: process_id,
        ProcessLoopbackMode: if include_process_tree {
            Audio::PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
        } else {
            Audio::PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
        },
    };
    let mut activation = Box::pin(PROPVARIANT::default());
    // SAFETY: we set the discriminant and matching union arm before publishing;
    // the blob points only to the owned pinned parameters, never a temporary.
    unsafe {
        let value = &mut *activation.as_mut().get_mut().Anonymous.Anonymous;
        value.vt = VT_BLOB;
        value.Anonymous.blob = BLOB {
            cbSize: size_of::<Audio::AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: std::ptr::from_mut(parameters.as_mut().get_mut()).cast(),
        };
    }

    Arc::new(ActivationLifetime {
        blob: activation,
        _parameters: parameters,
    })
}
fn activate_process_loopback(
    process_id: u32,
    include_process_tree: bool,
) -> Result<ProcessLoopbackActivation, NativeAudioError> {
    let lifetime = activation_lifetime(process_id, include_process_tree);
    let (sender, receiver) = std_mpsc::sync_channel(1);
    // The OS retains the handler through completion, even when our wait times out.
    // Keeping arguments in the handler prevents dangling activation pointers on that path.
    let handler: Audio::IActivateAudioInterfaceCompletionHandler =
        ActivationHandler(sender, Arc::clone(&lifetime)).into();
    // SAFETY: both argument pointees remain immutable/live through the OS-retained
    // completion handler, including activation failure or receiver timeout.
    let operation = unsafe {
        Audio::ActivateAudioInterfaceAsync(
            Audio::VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &Audio::IAudioClient::IID,
            Some(lifetime.blob.as_ref().get_ref()),
            &handler,
        )
    }
    .map_err(|error| {
        NativeAudioError::UnsupportedConfig(format!("activate WASAPI process loopback: {error}"))
    })?;
    let client = receiver
        .recv_timeout(ACTIVATION_TIMEOUT)
        .map_err(|error| NativeAudioError::Backend(format!("wait for WASAPI activation: {error}")))?
        .map_err(|error| {
            NativeAudioError::UnsupportedConfig(format!(
                "activate WASAPI process loopback: {error}"
            ))
        })?;
    Ok(ProcessLoopbackActivation {
        client,
        _operation: operation,
        _completion_handler: handler,
        _activation_lifetime: lifetime,
        _owner_thread: std::marker::PhantomData,
    })
}

#[windows::core::implement(Audio::IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler(
    std_mpsc::SyncSender<windows::core::Result<Audio::IAudioClient>>,
    Arc<ActivationLifetime>,
);

impl Audio::IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        operation: Ref<'_, Audio::IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let _keep_alive = &self.1;
        let result = operation
            .ok()
            .and_then(get_activation_result)
            .and_then(|interface| interface.cast());
        let _ = self.0.send(result);
        Ok(())
    }
}

fn get_activation_result(
    operation: &Audio::IActivateAudioInterfaceAsyncOperation,
) -> windows::core::Result<IUnknown> {
    let mut status = HRESULT(0);
    let mut interface = None;
    // SAFETY: operation is a live completion object; output pointers are valid owned locals.
    unsafe { operation.GetActivateResult(&mut status, &mut interface)? };
    status.ok()?;
    interface.ok_or_else(|| {
        WindowsError::new(
            Audio::AUDCLNT_E_DEVICE_INVALIDATED,
            "WASAPI activation returned no audio client",
        )
    })
}

fn capture_packets(
    state: &CaptureState,
    format: PcmFormat,
    ingress: &mut CaptureIngress,
    stop: &AtomicBool,
) -> Result<(), NativeAudioError> {
    let channels = usize::from(format.channels);
    let handles = [state._event.0, state._process.0];
    while !stop.load(Ordering::Acquire) {
        // SAFETY: handles remain owned/live by CaptureState on this worker.
        let status = unsafe { WaitForMultipleObjects(&handles, false, CAPTURE_WAIT_MS) };
        if status == WAIT_TIMEOUT {
            continue;
        }
        if status.0 == WAIT_OBJECT_0.0 + 1 {
            return Err(process_unavailable(state.process_id));
        }
        if status != WAIT_OBJECT_0 {
            return Err(NativeAudioError::Backend("WASAPI event wait failed".into()));
        }
        loop {
            // SAFETY: all capture operations occur on the original COM owner thread.
            let packet_frames = unsafe { state.capture.GetNextPacketSize() }
                .map_err(|e| NativeAudioError::Backend(format!("query WASAPI packet: {e}")))?;
            if packet_frames == 0 {
                break;
            }
            let (mut data, mut frames, mut flags, mut position, mut qpc) =
                (std::ptr::null_mut(), 0, 0, 0, 0);
            // SAFETY: output pointers are valid stack locations; buffer is read only until release.
            unsafe {
                state.capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    Some(&mut position),
                    Some(&mut qpc),
                )
            }
            .map_err(|e| NativeAudioError::Backend(format!("read WASAPI buffer: {e}")))?;
            let guard = CaptureBuffer {
                capture: &state.capture,
                frames,
            };
            let native_flags = decode_capture_flags(flags);
            if !native_flags.silent && data.is_null() {
                return Err(NativeAudioError::Backend(
                    "null non-silent WASAPI buffer".into(),
                ));
            }
            let samples = frames as usize * channels;
            let packet = if native_flags.silent {
                None
            } else {
                // SAFETY: requested PCM16 layout is aligned and GetBuffer keeps samples valid.
                Some(unsafe { slice::from_raw_parts(data.cast::<i16>(), samples) })
            };
            ingress.begin_callback();
            ingress.set_device_position(position);
            for index in 0..frames as usize {
                let mut converted = [0.0; 32];
                if let Some(packet) = packet {
                    for channel in 0..channels {
                        converted[channel] =
                            f32::from(packet[index * channels + channel]) / 32768.0;
                    }
                }
                let stamp = if native_flags.timestamp_error {
                    None
                } else {
                    qpc.checked_mul(100).and_then(|ns| {
                        ns.checked_add(index as u64 * 1_000_000_000 / u64::from(format.sample_rate))
                    })
                };
                let flags = CaptureFlags {
                    discontinuity: native_flags.discontinuity && index == 0,
                    ..native_flags
                };
                ingress.push(&converted[..channels], stamp, flags);
            }
            guard.release()?;
        }
    }
    Ok(())
}

fn decode_capture_flags(flags: u32) -> CaptureFlags {
    CaptureFlags {
        silent: flags & Audio::AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0,
        discontinuity: flags & Audio::AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0,
        timestamp_error: flags & Audio::AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0,
    }
}
struct CaptureBuffer<'a> {
    capture: &'a Audio::IAudioCaptureClient,
    frames: u32,
}
impl CaptureBuffer<'_> {
    fn release(mut self) -> Result<(), NativeAudioError> {
        let frames = std::mem::take(&mut self.frames);
        // SAFETY: exactly one same-thread ReleaseBuffer pairs the successful GetBuffer.
        unsafe { self.capture.ReleaseBuffer(frames) }
            .map_err(|e| NativeAudioError::Backend(format!("release WASAPI buffer: {e}")))
    }
}
impl Drop for CaptureBuffer<'_> {
    fn drop(&mut self) {
        if self.frames != 0 {
            // SAFETY: error-path release occurs before leaving the COM owner thread.
            let _ = unsafe { self.capture.ReleaseBuffer(self.frames) };
        }
    }
}
fn process_unavailable(process_id: u32) -> NativeAudioError {
    NativeAudioError::DeviceUnavailable {
        direction: DeviceDirection::Input,
        detail: format!("target process {process_id} has exited"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoned_activation_receiver_keeps_arguments_live_until_handler_release() {
        let lifetime = activation_lifetime(42, true);
        let weak = Arc::downgrade(&lifetime);
        // SAFETY: helper initialized this exact discriminant/union arm; allocations are pinned.
        let blob = unsafe { lifetime.blob.Anonymous.Anonymous.Anonymous.blob };
        assert_eq!(
            blob.pBlobData.cast_const(),
            std::ptr::from_ref(lifetime._parameters.as_ref().get_ref()).cast::<u8>()
        );
        let (sender, receiver) = std_mpsc::sync_channel(1);
        let handler: Audio::IActivateAudioInterfaceCompletionHandler =
            ActivationHandler(sender, Arc::clone(&lifetime)).into();
        drop(receiver);
        drop(lifetime);
        assert!(weak.upgrade().is_some());
        drop(handler);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn raw_capture_flags_are_independent_and_unknown_bits_do_not_become_gaps() {
        assert_eq!(decode_capture_flags(0x8000), CaptureFlags::default());
        let bits = Audio::AUDCLNT_BUFFERFLAGS_SILENT.0
            | Audio::AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0
            | Audio::AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0;
        assert_eq!(
            decode_capture_flags(bits as u32),
            CaptureFlags {
                silent: true,
                discontinuity: true,
                timestamp_error: true
            }
        );
    }

    #[test]
    fn activation_params_keep_process_and_tree_policy() {
        for include_process_tree in [false, true] {
            let mut parameters = Audio::AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: Audio::AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: Audio::AUDIOCLIENT_ACTIVATION_PARAMS_0::default(),
            };
            parameters.Anonymous.ProcessLoopbackParams =
                Audio::AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: 42,
                    ProcessLoopbackMode: if include_process_tree {
                        Audio::PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                    } else {
                        Audio::PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                    },
                };

            let actual = unsafe { parameters.Anonymous.ProcessLoopbackParams };
            assert_eq!(actual.TargetProcessId, 42);
            assert_eq!(
                actual.ProcessLoopbackMode,
                if include_process_tree {
                    Audio::PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                } else {
                    Audio::PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                }
            );
        }
    }

    #[test]
    fn capture_pcm_format_uses_negotiated_rate_and_layout() {
        let format = PcmFormat::new(48_000, 2, 20).unwrap();
        let wave = pcm16_wave_format(format);
        let actual = unsafe {
            (
                std::ptr::addr_of!(wave.wFormatTag).read_unaligned(),
                std::ptr::addr_of!(wave.nChannels).read_unaligned(),
                std::ptr::addr_of!(wave.nSamplesPerSec).read_unaligned(),
                std::ptr::addr_of!(wave.nBlockAlign).read_unaligned(),
                std::ptr::addr_of!(wave.wBitsPerSample).read_unaligned(),
                std::ptr::addr_of!(wave.cbSize).read_unaligned(),
            )
        };
        assert_eq!(actual, (Audio::WAVE_FORMAT_PCM as u16, 2, 48_000, 4, 16, 0));
    }

    #[test]
    fn process_capture_rejects_empty_or_self_targets_and_invalid_layouts() {
        let format = PcmFormat::new(48_000, 1, 20).unwrap();
        assert!(validate_request(0, format).is_err());
        assert!(validate_request(std::process::id(), format).is_err());
        assert!(validate_request(42, PcmFormat::new(48_000, 3, 20).unwrap()).is_err());
        assert!(validate_request(42, format).is_ok());
    }
}
