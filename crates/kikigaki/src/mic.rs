use anyhow::{bail, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use kikigaki_core::audio::{downmix_into, StreamResampler};
use kikigaki_core::engine::{AudioSink, EngineCmd, SinkError};
use kikigaki_core::protocol::SAMPLE_RATE;

pub struct Mic {
    stream: Option<cpal::Stream>,
}

impl Mic {
    pub fn new() -> Self {
        Self { stream: None }
    }

    pub fn start(&mut self, sink: AudioSink) -> anyhow::Result<()> {
        self.stop();

        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no default microphone is available")?;
        let preferred = device
            .supported_input_configs()
            .context("query microphone formats")?
            .find(|range| {
                range.channels() == 1
                    && range.sample_format() == SampleFormat::F32
                    && range.min_sample_rate() <= SAMPLE_RATE
                    && range.max_sample_rate() >= SAMPLE_RATE
            })
            .map(|range| range.with_sample_rate(SAMPLE_RATE));
        let supported = match preferred {
            Some(config) => config,
            None => device
                .default_input_config()
                .context("query default microphone format")?,
        };
        if supported.sample_format() != SampleFormat::F32 {
            bail!(
                "default microphone sample format {:?} is not f32",
                supported.sample_format()
            );
        }

        let config = supported.config();
        let input_rate = config.sample_rate;
        let channels = usize::from(config.channels);
        tracing::info!(
            input_rate,
            channels,
            resampling = input_rate != SAMPLE_RATE,
            "microphone stream format"
        );
        let mut resampler = StreamResampler::new(input_rate, SAMPLE_RATE);
        resampler.reset();
        let passthrough = channels == 1 && input_rate == SAMPLE_RATE;
        let mut mono_scratch = Vec::new();
        let mut resampled_scratch = Vec::new();
        let mut closed_warned = false;
        let stream = device
            .build_input_stream(
                config,
                move |data: &[f32], _| {
                    let samples = if passthrough {
                        data.to_vec()
                    } else {
                        mono_scratch.clear();
                        downmix_into(data, channels, &mut mono_scratch);
                        resampled_scratch.clear();
                        resampler.push_into(&mono_scratch, &mut resampled_scratch);
                        resampled_scratch.clone()
                    };
                    if samples.is_empty() {
                        return;
                    }
                    if matches!(sink.send(EngineCmd::Audio(samples)), Err(SinkError::Closed))
                        && !closed_warned
                    {
                        closed_warned = true;
                        tracing::warn!("engine command channel closed; microphone audio stopped");
                    }
                },
                |error| tracing::error!(%error, "microphone stream error"),
                None,
            )
            .context(
                "open microphone stream; grant Microphone access to kikigaki in System Settings",
            )?;
        stream.play().context(
            "start microphone stream; grant Microphone access to kikigaki in System Settings",
        )?;
        self.stream = Some(stream);
        Ok(())
    }

    pub fn stop(&mut self) {
        self.stream.take();
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.stop();
    }
}
