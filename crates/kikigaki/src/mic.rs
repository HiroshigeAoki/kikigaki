use crate::mic_format;
use crate::mic_gate::{Gate, Shared};
use anyhow::{bail, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use kikigaki_core::audio::{downmix_into, StreamResampler};
use kikigaki_core::engine::{AudioSink, EngineCmd, SinkError};
use kikigaki_core::protocol::SAMPLE_RATE;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WARM_STREAM: bool = cfg!(target_os = "windows");
const STREAM_BUILD_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Mic {
    stream: Option<cpal::Stream>,
    shared: Option<Arc<Shared>>,
    device_name: Option<String>,
}

struct BuiltStream {
    stream: cpal::Stream,
    shared: Arc<Shared>,
    host_ms: u128,
    device_ms: u128,
    configs_ms: u128,
    build_ms: u128,
    entry_at: Instant,
    input_rate: u32,
    channels: usize,
    sample_format: SampleFormat,
    device_name: Option<String>,
}

fn format_rank(format: SampleFormat) -> Option<u8> {
    let format = match format {
        SampleFormat::F32 => mic_format::SampleFormat::F32,
        SampleFormat::I16 => mic_format::SampleFormat::I16,
        SampleFormat::I32 => mic_format::SampleFormat::I32,
        SampleFormat::I24 => mic_format::SampleFormat::I24,
        SampleFormat::U16 => mic_format::SampleFormat::U16,
        SampleFormat::I8 => mic_format::SampleFormat::I8,
        SampleFormat::U8 => mic_format::SampleFormat::U8,
        SampleFormat::U24 => mic_format::SampleFormat::U24,
        SampleFormat::U32 => mic_format::SampleFormat::U32,
        SampleFormat::I64 => mic_format::SampleFormat::I64,
        SampleFormat::U64 => mic_format::SampleFormat::U64,
        SampleFormat::F64 => mic_format::SampleFormat::F64,
        _ => return None,
    };
    mic_format::format_rank(format)
}

fn build_sample_stream<T, F, E>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut on_samples: F,
    on_error: E,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
    F: FnMut(Vec<f32>) + Send + 'static,
    E: FnMut(cpal::Error) + Send + 'static,
{
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            on_samples(data.iter().copied().map(f32::from_sample).collect());
        },
        on_error,
        WARM_STREAM.then_some(STREAM_BUILD_TIMEOUT),
    )
}

/// Builds an input stream for the device's actual sample format and normalizes samples to f32.
fn build_capture_stream<F, E>(
    device: &cpal::Device,
    supported: &cpal::SupportedStreamConfig,
    on_samples: F,
    on_error: E,
) -> Result<cpal::Stream, cpal::Error>
where
    F: FnMut(Vec<f32>) + Send + 'static,
    E: FnMut(cpal::Error) + Send + 'static,
{
    let config = supported.config();
    match supported.sample_format() {
        SampleFormat::I8 => build_sample_stream::<i8, _, _>(device, config, on_samples, on_error),
        SampleFormat::I16 => build_sample_stream::<i16, _, _>(device, config, on_samples, on_error),
        SampleFormat::I24 => {
            build_sample_stream::<cpal::I24, _, _>(device, config, on_samples, on_error)
        }
        SampleFormat::I32 => build_sample_stream::<i32, _, _>(device, config, on_samples, on_error),
        SampleFormat::I64 => build_sample_stream::<i64, _, _>(device, config, on_samples, on_error),
        SampleFormat::U8 => build_sample_stream::<u8, _, _>(device, config, on_samples, on_error),
        SampleFormat::U16 => build_sample_stream::<u16, _, _>(device, config, on_samples, on_error),
        SampleFormat::U24 => {
            build_sample_stream::<cpal::U24, _, _>(device, config, on_samples, on_error)
        }
        SampleFormat::U32 => build_sample_stream::<u32, _, _>(device, config, on_samples, on_error),
        SampleFormat::U64 => build_sample_stream::<u64, _, _>(device, config, on_samples, on_error),
        SampleFormat::F32 => build_sample_stream::<f32, _, _>(device, config, on_samples, on_error),
        SampleFormat::F64 => build_sample_stream::<f64, _, _>(device, config, on_samples, on_error),
        format => Err(cpal::Error::with_message(
            cpal::ErrorKind::UnsupportedConfig,
            format!("unsupported microphone sample format {format:?}"),
        )),
    }
}

/// Resolves the default input to a concrete Windows endpoint when possible.
///
/// A `Specific` endpoint avoids the virtual default handle's asynchronous activation, but it does
/// not follow later Windows default-device changes. The per-press name check in `start`, together
/// with rebuilding after stream errors, is the recovery path. cpal exposes no endpoint ID for the
/// virtual default handle, so two endpoints with the same friendly name bind the first enumerated
/// one; the per-press check bounds the damage when the displayed default name changes.
fn resolve_input_device(host: &cpal::Host) -> anyhow::Result<(cpal::Device, Option<String>)> {
    let default = host
        .default_input_device()
        .context("no default microphone is available")?;

    if !WARM_STREAM {
        return Ok((default, None));
    }

    let default_name = default
        .description()
        .ok()
        .map(|description| description.name().to_owned());
    let log_name = default_name.as_deref().unwrap_or("<unknown>");

    if let Some(default_name) = default_name.as_deref() {
        match host.input_devices() {
            Ok(devices) => {
                for device in devices {
                    if device
                        .description()
                        .is_ok_and(|description| description.name() == default_name)
                    {
                        tracing::info!(name = %default_name, concrete = true, "microphone device resolved");
                        return Ok((device, Some(default_name.to_owned())));
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "enumerate concrete microphones");
            }
        }
    }

    tracing::warn!(
        name = %log_name,
        "using virtual default microphone; activation may be slow"
    );
    tracing::info!(name = %log_name, concrete = false, "microphone device resolved");
    Ok((default, default_name))
}

fn default_input_device_name(host: &cpal::Host) -> Option<String> {
    host.default_input_device()?
        .description()
        .ok()
        .map(|description| description.name().to_owned())
}

impl Mic {
    pub fn new() -> Self {
        Self {
            stream: None,
            shared: None,
            device_name: None,
        }
    }

    /// Prepares a paused stream for the current Windows default microphone.
    ///
    /// The resolved `Specific` endpoint does not follow later default-device changes; `start`
    /// rechecks the friendly name on every press, and stream errors also force a rebuild.
    pub fn prepare(&mut self) -> anyhow::Result<()> {
        if !WARM_STREAM {
            return Ok(());
        }
        if self.stream.is_some()
            && self
                .shared
                .as_ref()
                .is_some_and(|shared| !shared.needs_rebuild())
        {
            return Ok(());
        }
        self.discard_stream();
        let built = Self::build_stream()?;
        tracing::info!(
            host_ms = built.host_ms,
            device_ms = built.device_ms,
            configs_ms = built.configs_ms,
            build_ms = built.build_ms,
            total_ms = built.entry_at.elapsed().as_millis(),
            input_rate = built.input_rate,
            channels = built.channels,
            sample_format = ?built.sample_format,
            "microphone stream prepared"
        );
        self.stream = Some(built.stream);
        self.shared = Some(built.shared);
        self.device_name = built.device_name;
        Ok(())
    }

    /// Starts capture for one press.
    ///
    /// On cpal 0.18.2's WASAPI backend, `play()` and `pause()` only send commands to the stream
    /// thread; `IAudioClient::Start()` and `Stop()` run asynchronously and failures arrive through
    /// the error callback. Thus the `play()` error branch only catches an already-dead stream,
    /// `play_ms` measures the channel send, and `since_play_ms` on `microphone first callback` is
    /// the real start latency.
    pub fn start(&mut self, sink: AudioSink) -> anyhow::Result<()> {
        self.stop();
        let started = Instant::now();

        if WARM_STREAM {
            let mut warm = self.stream.is_some()
                && self
                    .shared
                    .as_ref()
                    .is_some_and(|shared| !shared.needs_rebuild());
            if warm {
                let host = cpal::default_host();
                if let Some(current) = default_input_device_name(&host) {
                    if self.device_name.as_ref() != Some(&current) {
                        let stored = self.device_name.as_deref().unwrap_or("<unknown>");
                        tracing::info!(old = %stored, new = %current, "default microphone changed; rebuilding stream");
                        self.discard_stream();
                        warm = false;
                    }
                }
            }
            if !warm {
                self.prepare()?;
            }
            let shared = Arc::clone(
                self.shared
                    .as_ref()
                    .expect("prepare installed shared state"),
            );
            shared.arm(sink);
            let play_started_at = Instant::now();
            if let Err(error) = self
                .stream
                .as_ref()
                .expect("prepare installed stream")
                .play()
            {
                self.discard_stream();
                return Err(error).context("start microphone stream");
            }
            tracing::info!(
                open_ms = started.elapsed().as_millis(),
                play_ms = play_started_at.elapsed().as_millis(),
                warm,
                "microphone stream started"
            );
            return Ok(());
        }

        let built = Self::build_stream()?;
        let build_ms = built.build_ms;
        let entry_at = built.entry_at;
        self.stream = Some(built.stream);
        self.shared = Some(built.shared);
        let shared = Arc::clone(self.shared.as_ref().expect("build installed shared state"));
        shared.arm(sink);
        let play_started_at = Instant::now();
        if let Err(error) = self.stream.as_ref().expect("build installed stream").play() {
            self.discard_stream();
            return Err(error).context(
                "start microphone stream; grant Microphone access to kikigaki in System Settings",
            );
        }
        tracing::info!(
            build_ms,
            play_ms = play_started_at.elapsed().as_millis(),
            open_ms = entry_at.elapsed().as_millis(),
            warm = false,
            "microphone stream started"
        );
        Ok(())
    }

    fn build_stream() -> anyhow::Result<BuiltStream> {
        let entry_at = Instant::now();

        let host_started_at = Instant::now();
        let host = cpal::default_host();
        let host_finished_at = Instant::now();
        let (device, device_name) = resolve_input_device(&host)?;
        let device_finished_at = Instant::now();
        let preferred = device
            .supported_input_configs()
            .context("query microphone formats")?
            .filter_map(|range| {
                if range.channels() != 1 || !range.contains_rate(SAMPLE_RATE) {
                    return None;
                }
                let rank = format_rank(range.sample_format())?;
                Some((rank, range))
            })
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, range)| range.with_sample_rate(SAMPLE_RATE));
        let supported = match preferred {
            Some(config) => config,
            None => device
                .default_input_config()
                .context("query default microphone format")?,
        };
        let configs_finished_at = Instant::now();
        let config = supported.config();
        let input_rate = config.sample_rate;
        let channels = usize::from(config.channels);
        tracing::info!(
            input_rate,
            channels,
            resampling = input_rate != SAMPLE_RATE,
            sample_format = ?supported.sample_format(),
            buffer_size = ?config.buffer_size,
            host_ms = host_finished_at.duration_since(host_started_at).as_millis(),
            device_ms = device_finished_at.duration_since(host_finished_at).as_millis(),
            configs_ms = configs_finished_at.duration_since(device_finished_at).as_millis(),
            "microphone stream format"
        );
        let mut resampler = StreamResampler::new(input_rate, SAMPLE_RATE);
        let passthrough = channels == 1 && input_rate == SAMPLE_RATE;
        let mut mono_scratch = Vec::new();
        let mut resampled_scratch = Vec::new();
        let mut closed_warned = false;
        let shared = Arc::new(Shared::default());
        let capture_shared = Arc::clone(&shared);
        let error_shared = Arc::clone(&shared);
        let format = supported.sample_format();
        if format_rank(format).is_none() {
            bail!("microphone sample format {format:?} is not supported");
        }
        let build_started_at = Instant::now();
        let stream = build_capture_stream(
            &device,
            &supported,
            move |data| {
                let Gate::Process { reset, generation } = capture_shared.gate_buffer() else {
                    return;
                };
                if reset {
                    resampler.reset();
                    mono_scratch.clear();
                    resampled_scratch.clear();
                    closed_warned = false;
                }
                if !capture_shared.first_callback.swap(true, Ordering::Relaxed) {
                    let since_play_ms = capture_shared
                        .play_started_at
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map_or(0, |started_at| started_at.elapsed().as_millis());
                    tracing::info!(
                        samples = data.len(),
                        since_build_ms = build_started_at.elapsed().as_millis(),
                        since_play_ms,
                        "microphone first callback"
                    );
                }
                let samples = if passthrough {
                    data
                } else {
                    mono_scratch.clear();
                    downmix_into(&data, channels, &mut mono_scratch);
                    resampled_scratch.clear();
                    resampler.push_into(&mono_scratch, &mut resampled_scratch);
                    resampled_scratch.clone()
                };
                if samples.is_empty() {
                    return;
                }
                // WASAPI keeps un-read packets across Stop/Start, so up to one period of stale
                // audio can be delivered after the next Start. This generation check plus the
                // engine-side `current.is_some()` guard bound the effect. Holding the sink lock
                // through the send also ensures no packet is sent after Stop returns.
                let Some(sink) = capture_shared.lock_sink_for_generation(generation) else {
                    return;
                };
                if sink.as_ref().is_some_and(|sink| {
                    matches!(sink.send(EngineCmd::Audio(samples)), Err(SinkError::Closed))
                }) {
                    if !closed_warned {
                        closed_warned = true;
                        tracing::warn!("engine command channel closed; microphone audio stopped");
                    }
                }
            },
            move |error| {
                error_shared.record_stream_error(error.kind() == cpal::ErrorKind::Xrun, &error)
            },
        )
        .context(if WARM_STREAM {
            "open microphone stream"
        } else {
            "open microphone stream; grant Microphone access to kikigaki in System Settings"
        })?;
        let build_finished_at = Instant::now();
        Ok(BuiltStream {
            stream,
            shared,
            host_ms: host_finished_at.duration_since(host_started_at).as_millis(),
            device_ms: device_finished_at
                .duration_since(host_finished_at)
                .as_millis(),
            configs_ms: configs_finished_at
                .duration_since(device_finished_at)
                .as_millis(),
            build_ms: build_finished_at
                .duration_since(build_started_at)
                .as_millis(),
            entry_at,
            input_rate,
            channels,
            sample_format: format,
            device_name,
        })
    }

    pub fn stop(&mut self) {
        let Some(shared) = self.shared.as_ref().map(Arc::clone) else {
            self.stream.take();
            return;
        };
        let was_active = shared.active.swap(false, Ordering::SeqCst);
        shared.clear_sink();

        let mut pause_failed = None;
        let pause_started_at = Instant::now();
        if was_active && WARM_STREAM {
            if let Some(stream) = self.stream.as_ref() {
                if let Err(error) = stream.pause() {
                    pause_failed = Some(error);
                }
            }
        }

        let rebuild = shared.needs_rebuild();
        if was_active {
            if !shared.first_callback.load(Ordering::Relaxed) && rebuild {
                tracing::warn!("press captured no audio; microphone stream will be rebuilt");
            }
            if WARM_STREAM {
                tracing::info!(
                    pause_ms = pause_started_at.elapsed().as_millis(),
                    kept_warm = pause_failed.is_none() && !rebuild,
                    "microphone stream stopped"
                );
            }
        }

        if let Some(error) = pause_failed {
            tracing::warn!(%error, "microphone pause failed; discarding stream");
            self.discard_stream();
        } else if rebuild {
            tracing::warn!("microphone stream errored; discarding stream");
            self.discard_stream();
        } else if !WARM_STREAM {
            self.discard_stream();
        }
    }

    fn discard_stream(&mut self) {
        if let Some(shared) = self.shared.as_ref() {
            shared.active.store(false, Ordering::SeqCst);
            shared.clear_sink();
        }
        self.stream.take();
        self.shared.take();
        self.device_name.take();
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.stop();
        self.discard_stream();
    }
}
