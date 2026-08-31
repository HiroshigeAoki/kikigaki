use std::collections::VecDeque;

const LOWER_RATE_HALF_TAPS: f64 = 32.0;
const CUTOFF_FRACTION: f64 = 0.43;
const MAX_POLYPHASE_PERIOD: u32 = 1_024;
const MAX_POLYPHASE_COEFFICIENTS: usize = 1_000_000;

struct CoefficientRow {
    first_offset: i64,
    weights: Vec<f64>,
    weight_sum: f64,
}

/// Stateful anti-aliased resampler that preserves filter history across chunks.
///
/// The windowed-sinc FIR is shifted into the past so `push` never needs samples
/// from a future chunk. This introduces a group delay of approximately 32
/// samples at the lower of the input and output rates (2 ms at 16 kHz).
pub struct StreamResampler {
    from_rate: u32,
    to_rate: u32,
    next_output_index: u64,
    input_len: u64,
    history_start: u64,
    history: VecDeque<f32>,
    half_width: f64,
    cutoff: f64,
    coefficient_rows: Option<Vec<CoefficientRow>>,
}

impl StreamResampler {
    /// Creates a resampler from `from_rate` to `to_rate` samples per second.
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        let lower_rate = from_rate.min(to_rate);
        let (half_width, cutoff) = if lower_rate == 0 {
            (0.0, 0.0)
        } else {
            (
                (LOWER_RATE_HALF_TAPS * f64::from(from_rate) / f64::from(lower_rate)).ceil(),
                CUTOFF_FRACTION * f64::from(lower_rate) / f64::from(from_rate),
            )
        };
        let period = if from_rate == 0 || to_rate == 0 {
            0
        } else {
            to_rate / gcd(from_rate, to_rate)
        };
        let taps_per_phase = (2.0 * half_width + 1.0) as usize;
        // At most 1,024 phase rows and 1,000,000 f64 coefficients (~8 MiB, plus row metadata)
        // are retained. Unusual ratios beyond either bound use the direct formula instead.
        let coefficient_rows = (from_rate != to_rate
            && period <= MAX_POLYPHASE_PERIOD
            && usize::try_from(period)
                .ok()
                .and_then(|period| period.checked_mul(taps_per_phase))
                .is_some_and(|count| count <= MAX_POLYPHASE_COEFFICIENTS))
        .then(|| {
            (0..period)
                .map(|phase| {
                    coefficient_row(
                        f64::from(phase) * f64::from(from_rate) / f64::from(to_rate),
                        half_width,
                        cutoff,
                    )
                })
                .collect()
        });
        Self {
            from_rate,
            to_rate,
            next_output_index: 0,
            input_len: 0,
            history_start: 0,
            history: VecDeque::new(),
            half_width,
            cutoff,
            coefficient_rows,
        }
    }

    /// Resamples the next contiguous mono input chunk.
    pub fn push(&mut self, samples: &[f32]) -> Vec<f32> {
        let mut output = Vec::with_capacity(self.output_capacity(samples.len()));
        self.push_into(samples, &mut output);
        output
    }

    /// Resamples the next contiguous mono input chunk and appends it to `output`.
    pub fn push_into(&mut self, samples: &[f32], output: &mut Vec<f32>) {
        if samples.is_empty() || self.from_rate == 0 || self.to_rate == 0 {
            return;
        }
        if self.from_rate == self.to_rate {
            self.input_len = self.input_len.saturating_add(samples.len() as u64);
            output.extend_from_slice(samples);
            return;
        }

        self.history.extend(samples.iter().copied());
        self.input_len = self.input_len.saturating_add(samples.len() as u64);
        while u128::from(self.next_output_index) * u128::from(self.from_rate)
            < u128::from(self.input_len) * u128::from(self.to_rate)
        {
            let source_position =
                self.next_output_index as f64 * f64::from(self.from_rate) / f64::from(self.to_rate);
            output.push(self.filtered_sample(source_position, self.next_output_index));
            self.next_output_index = self.next_output_index.saturating_add(1);
        }

        self.discard_unused_history();
    }

    fn output_capacity(&self, input_count: usize) -> usize {
        if self.from_rate == 0 || self.to_rate == 0 {
            return 0;
        }
        if self.from_rate == self.to_rate {
            return input_count;
        }
        let future_input = self.input_len.saturating_add(input_count as u64);
        let numerator = u128::from(future_input) * u128::from(self.to_rate);
        let output_end = numerator.div_ceil(u128::from(self.from_rate));
        usize::try_from(output_end.saturating_sub(u128::from(self.next_output_index)))
            .unwrap_or(input_count)
    }

    fn filtered_sample(&self, source_position: f64, output_index: u64) -> f32 {
        if let Some(rows) = &self.coefficient_rows {
            let row = &rows[output_index as usize % rows.len()];
            let base = source_position.floor() as i64;
            let mut weighted_sum = 0.0;
            for (offset, weight) in row.weights.iter().enumerate() {
                let index = base + row.first_offset + offset as i64;
                if index >= 0 {
                    weighted_sum += f64::from(self.sample(index as u64)) * weight;
                }
            }
            return (weighted_sum / row.weight_sum) as f32;
        }
        self.filtered_sample_formula(source_position)
    }

    fn filtered_sample_formula(&self, source_position: f64) -> f32 {
        let center = source_position - self.half_width;
        let first = (center - self.half_width).ceil() as i64;
        let last = (center + self.half_width).floor() as i64;
        let mut weighted_sum = 0.0;
        let mut weight_sum = 0.0;

        for index in first..=last {
            let distance = center - index as f64;
            let weight = low_pass_kernel(distance, self.half_width, self.cutoff);
            weight_sum += weight;
            if index >= 0 {
                weighted_sum += f64::from(self.sample(index as u64)) * weight;
            }
        }

        debug_assert!(weight_sum.abs() > f64::EPSILON);
        (weighted_sum / weight_sum) as f32
    }

    #[cfg(test)]
    fn push_direct_for_test(&mut self, samples: &[f32]) -> Vec<f32> {
        if samples.is_empty() || self.from_rate == 0 || self.to_rate == 0 {
            return Vec::new();
        }
        if self.from_rate == self.to_rate {
            self.input_len = self.input_len.saturating_add(samples.len() as u64);
            return samples.to_vec();
        }

        self.history.extend(samples.iter().copied());
        self.input_len = self.input_len.saturating_add(samples.len() as u64);
        let mut output = Vec::with_capacity(self.output_capacity(0));
        while u128::from(self.next_output_index) * u128::from(self.from_rate)
            < u128::from(self.input_len) * u128::from(self.to_rate)
        {
            let source_position =
                self.next_output_index as f64 * f64::from(self.from_rate) / f64::from(self.to_rate);
            output.push(self.filtered_sample_reference(source_position));
            self.next_output_index = self.next_output_index.saturating_add(1);
        }
        self.discard_unused_history();
        output
    }

    #[cfg(test)]
    fn filtered_sample_reference(&self, source_position: f64) -> f32 {
        let center = source_position - self.half_width;
        let first = (center - self.half_width).ceil() as i64;
        let last = (center + self.half_width).floor() as i64;
        let mut weighted_sum = 0.0;
        let mut weight_sum = 0.0;

        for index in first..=last {
            let distance = center - index as f64;
            let weight = low_pass_kernel(distance, self.half_width, self.cutoff);
            weight_sum += weight;
            if index >= 0 {
                weighted_sum += f64::from(self.sample(index as u64)) * weight;
            }
        }

        debug_assert!(weight_sum.abs() > f64::EPSILON);
        (weighted_sum / weight_sum) as f32
    }

    fn sample(&self, index: u64) -> f32 {
        let offset = index
            .checked_sub(self.history_start)
            .expect("resampler retained required filter history");
        self.history[offset as usize]
    }

    fn discard_unused_history(&mut self) {
        let next_source_position =
            self.next_output_index as f64 * f64::from(self.from_rate) / f64::from(self.to_rate);
        let keep_from = if next_source_position > 2.0 * self.half_width {
            (next_source_position - 2.0 * self.half_width).floor() as u64
        } else {
            0
        };
        while self.history_start < keep_from {
            self.history.pop_front();
            self.history_start += 1;
        }
    }

    /// Clears carried samples and interpolation phase for a new stream.
    pub fn reset(&mut self) {
        self.next_output_index = 0;
        self.input_len = 0;
        self.history_start = 0;
        self.history.clear();
    }
}

fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

fn coefficient_row(source_position: f64, half_width: f64, cutoff: f64) -> CoefficientRow {
    let center = source_position - half_width;
    let base = source_position.floor() as i64;
    let formula_first = (center - half_width).ceil() as i64;
    let first = base - 2 * half_width as i64;
    let last = base;
    let weights = (first..=last)
        .map(|index| {
            if index < formula_first {
                0.0
            } else {
                low_pass_kernel(center - index as f64, half_width, cutoff)
            }
        })
        .collect::<Vec<_>>();
    let weight_sum = weights.iter().sum();
    CoefficientRow {
        first_offset: first - base,
        weights,
        weight_sum,
    }
}

fn low_pass_kernel(distance: f64, half_width: f64, cutoff: f64) -> f64 {
    let sinc_argument = 2.0 * cutoff * distance;
    let sinc = if sinc_argument.abs() < f64::EPSILON {
        1.0
    } else {
        (std::f64::consts::PI * sinc_argument).sin() / (std::f64::consts::PI * sinc_argument)
    };
    let phase = std::f64::consts::PI * distance / half_width;
    let window = 0.35875
        + 0.48829 * phase.cos()
        + 0.14128 * (2.0 * phase).cos()
        + 0.01168 * (3.0 * phase).cos();
    2.0 * cutoff * sinc * window
}

/// Converts normalized floating-point samples to signed 16-bit little-endian PCM.
pub fn f32_to_s16le(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * size_of::<i16>());
    for &sample in samples {
        let sample = sample.clamp(-1.0, 1.0);
        let pcm = if sample <= -1.0 {
            i16::MIN
        } else {
            (sample * f32::from(i16::MAX)) as i16
        };
        bytes.extend_from_slice(&pcm.to_le_bytes());
    }
    bytes
}

/// Produces zero-valued signed 16-bit PCM for the requested duration.
pub fn silence_s16le(ms: u64, sample_rate: u32) -> Vec<u8> {
    let sample_count = ms.saturating_mul(u64::from(sample_rate)) / 1_000;
    let byte_count = usize::try_from(sample_count)
        .unwrap_or(usize::MAX / size_of::<i16>())
        .saturating_mul(size_of::<i16>());
    vec![0; byte_count]
}

/// Averages interleaved input channels into mono samples.
pub fn downmix(samples: &[f32], channels: usize) -> Vec<f32> {
    let mut output = Vec::with_capacity(samples.len().checked_div(channels).unwrap_or(0));
    downmix_into(samples, channels, &mut output);
    output
}

/// Averages interleaved input channels and appends the mono samples to `output`.
pub fn downmix_into(samples: &[f32], channels: usize, output: &mut Vec<f32>) {
    if channels == 0 {
        return;
    }
    if channels == 1 {
        output.extend_from_slice(samples);
        return;
    }

    output.extend(
        samples
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(sample_rate: u32, frequency: f32, seconds: u32) -> Vec<f32> {
        (0..sample_rate * seconds)
            .map(|index| {
                (index as f32 * frequency * std::f32::consts::TAU / sample_rate as f32).sin()
            })
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn f32_to_s16le_clamps_and_is_little_endian() {
        let b = f32_to_s16le(&[0.0, 1.0, -1.0, 2.0, 0.5]);
        assert_eq!(b.len(), 10);
        assert_eq!(&b[0..2], &[0, 0]);
        assert_eq!(&b[2..4], &0x7fffi16.to_le_bytes());
        assert_eq!(&b[4..6], &(-0x8000i16).to_le_bytes());
        assert_eq!(&b[6..8], &0x7fffi16.to_le_bytes());
        assert_eq!(i16::from_le_bytes([b[8], b[9]]), 16383);
    }

    #[test]
    fn silence_bytes_is_sample_rate_times_ms() {
        assert_eq!(silence_s16le(500, 16_000).len(), 16_000 / 2 * 2);
        assert!(silence_s16le(10, 16_000).iter().all(|&x| x == 0));
    }

    #[test]
    fn downmix_stereo_averages_pairs() {
        assert_eq!(downmix(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
        assert_eq!(downmix(&[1.0, 2.0], 1), vec![1.0, 2.0]);
    }

    #[test]
    fn stream_resampler_matches_one_shot_across_chunk_boundaries() {
        let input: Vec<f32> = (0..48_000)
            .map(|index| (index as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin())
            .collect();
        let mut one_shot = StreamResampler::new(48_000, 16_000);
        let expected = one_shot.push(&input);

        let mut chunked = StreamResampler::new(48_000, 16_000);
        let mut actual = Vec::new();
        let chunk_sizes = [1, 7, 480, 512, 1_000];
        let mut start = 0;
        for size in chunk_sizes.into_iter().cycle() {
            if start == input.len() {
                break;
            }
            let end = (start + size).min(input.len());
            actual.extend(chunked.push(&input[start..end]));
            start = end;
        }

        assert_eq!(actual.len(), expected.len());
        let max_difference = actual
            .iter()
            .zip(&expected)
            .map(|(actual, expected)| (actual - expected).abs())
            .fold(0.0_f32, f32::max);
        assert!(max_difference < 1e-5, "max difference {max_difference}");
    }

    #[test]
    fn stream_resampler_output_length_tracks_duration() {
        let input: Vec<f32> = (0..48_000)
            .map(|index| (index as f32 / 100.0).sin())
            .collect();
        let mut resampler = StreamResampler::new(48_000, 16_000);
        let output = resampler.push(&input);
        assert!((output.len() as i64 - 16_000).abs() <= 1);

        resampler.reset();
        assert_eq!(resampler.push(&input), output);
    }

    #[test]
    fn stream_resampler_attenuates_stopband_by_30_db() {
        let input = sine(48_000, 7_500.0, 1);
        let mut resampler = StreamResampler::new(48_000, 16_000);
        let output = resampler.push(&input);
        let attenuation_db = 20.0 * (rms(&output) / rms(&input)).log10();

        assert!(
            attenuation_db <= -30.0,
            "stopband attenuation was {attenuation_db:.1} dB"
        );
    }

    #[test]
    fn stream_resampler_keeps_passband_rms() {
        let input = sine(48_000, 1_000.0, 1);
        let mut resampler = StreamResampler::new(48_000, 16_000);
        let output = resampler.push(&input);
        let rms_ratio = rms(&output) / rms(&input);

        assert!(
            (0.95..=1.05).contains(&rms_ratio),
            "passband RMS ratio was {rms_ratio:.3}"
        );
    }

    #[test]
    fn stream_resampler_44k1_to_16k_keeps_frequency_and_rms() {
        let input = sine(44_100, 1_000.0, 1);
        let mut resampler = StreamResampler::new(44_100, 16_000);
        let output = resampler.push(&input);
        let mut polarity = 0_i8;
        let mut zero_crossings = 0_usize;
        for &sample in &output {
            let next_polarity = if sample > 0.001 {
                1
            } else if sample < -0.001 {
                -1
            } else {
                0
            };
            if next_polarity != 0 {
                if polarity != 0 && next_polarity != polarity {
                    zero_crossings += 1;
                }
                polarity = next_polarity;
            }
        }
        let rms_ratio = rms(&output) / rms(&input);

        assert!(
            zero_crossings.abs_diff(2_000) <= 2,
            "zero-crossing count was {zero_crossings}"
        );
        assert!(
            (0.95..=1.05).contains(&rms_ratio),
            "passband RMS ratio was {rms_ratio:.3}"
        );
    }

    #[test]
    fn stream_resampler_reset_discards_filter_history() {
        let stale = sine(48_000, 7_500.0, 1);
        let fresh_input = sine(48_000, 1_000.0, 1);
        let mut reset_resampler = StreamResampler::new(48_000, 16_000);
        let _ = reset_resampler.push(&stale[..12_345]);
        reset_resampler.reset();

        let mut fresh_resampler = StreamResampler::new(48_000, 16_000);
        assert_eq!(
            reset_resampler.push(&fresh_input),
            fresh_resampler.push(&fresh_input)
        );
    }

    #[test]
    fn stream_resampler_identity_is_exact() {
        let input = vec![0.1, -0.25, 0.5, 1.0];
        let mut resampler = StreamResampler::new(16_000, 16_000);
        assert_eq!(resampler.push(&input), input);
    }

    #[test]
    fn polyphase_table_matches_direct_formula() {
        for (from_rate, to_rate) in [(48_000, 16_000), (44_100, 16_000), (16_000, 16_000)] {
            let input = (0..4_096)
                .map(|index| {
                    let position = index as f32 / from_rate as f32;
                    let phase =
                        std::f32::consts::TAU * (180.0 * position + 3_800.0 * position * position);
                    phase.sin()
                })
                .collect::<Vec<_>>();
            let mut table = StreamResampler::new(from_rate, to_rate);
            let mut direct = StreamResampler::new(from_rate, to_rate);
            let actual = table.push(&input);
            let expected = direct.push_direct_for_test(&input);

            assert_eq!(actual.len(), expected.len());
            let max_difference = actual
                .iter()
                .zip(expected)
                .map(|(actual, expected)| (actual - expected).abs())
                .fold(0.0_f32, f32::max);
            assert!(
                max_difference <= 1e-6,
                "{from_rate}->{to_rate} max difference {max_difference}"
            );
        }
    }
}
