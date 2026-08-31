pub(crate) struct Framer {
    frame_size: usize,
    carry: Vec<f32>,
}

impl Framer {
    pub(crate) fn new(frame_size: usize) -> Self {
        assert!(frame_size > 0, "frame size must be positive");
        Self {
            frame_size,
            carry: Vec::with_capacity(frame_size),
        }
    }

    pub(crate) fn push(&mut self, mut samples: &[f32], mut emit: impl FnMut(&[f32])) {
        if !self.carry.is_empty() {
            let needed = self.frame_size - self.carry.len();
            let copied = needed.min(samples.len());
            self.carry.extend_from_slice(&samples[..copied]);
            samples = &samples[copied..];
            if self.carry.len() == self.frame_size {
                emit(&self.carry);
                self.carry.clear();
            }
        }

        let mut chunks = samples.chunks_exact(self.frame_size);
        for frame in &mut chunks {
            emit(frame);
        }
        self.carry.extend_from_slice(chunks.remainder());
    }

    pub(crate) fn flush_zero_padded(&mut self, mut emit: impl FnMut(&[f32])) {
        if self.carry.is_empty() {
            return;
        }
        self.carry.resize(self.frame_size, 0.0);
        emit(&self.carry);
        self.carry.clear();
    }

    pub(crate) fn reset(&mut self) {
        self.carry.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::Framer;

    fn frames_after_push(samples: &[f32]) -> (Framer, Vec<Vec<f32>>) {
        let mut framer = Framer::new(512);
        let mut frames = Vec::new();
        framer.push(samples, |frame| frames.push(frame.to_vec()));
        (framer, frames)
    }

    #[test]
    fn carries_511_samples() {
        let (_, frames) = frames_after_push(&vec![1.0; 511]);
        assert!(frames.is_empty());
    }

    #[test]
    fn emits_exact_frame_at_512_samples() {
        let samples: Vec<f32> = (0..512).map(|sample| sample as f32).collect();
        let (_, frames) = frames_after_push(&samples);
        assert_eq!(frames, vec![samples]);
    }

    #[test]
    fn emits_one_frame_and_carries_one_at_513_samples() {
        let samples: Vec<f32> = (0..513).map(|sample| sample as f32).collect();
        let (mut framer, frames) = frames_after_push(&samples);
        assert_eq!(frames, vec![samples[..512].to_vec()]);

        let mut flushed = Vec::new();
        framer.flush_zero_padded(|frame| flushed.push(frame.to_vec()));
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0][0], 512.0);
        assert!(flushed[0][1..].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn emits_two_frames_and_carries_one_at_1025_samples() {
        let samples: Vec<f32> = (0..1025).map(|sample| sample as f32).collect();
        let (mut framer, frames) = frames_after_push(&samples);
        assert_eq!(
            frames,
            vec![samples[..512].to_vec(), samples[512..1024].to_vec()]
        );

        let mut flushed = Vec::new();
        framer.flush_zero_padded(|frame| flushed.push(frame.to_vec()));
        assert_eq!(flushed[0][0], 1024.0);
    }

    #[test]
    fn flush_emits_only_for_nonempty_carry_and_clears_it() {
        let (mut framer, _) = frames_after_push(&vec![2.0; 100]);
        let mut frames = Vec::new();
        framer.flush_zero_padded(|frame| frames.push(frame.to_vec()));
        framer.flush_zero_padded(|frame| frames.push(frame.to_vec()));
        assert_eq!(frames.len(), 1);
        assert_eq!(&frames[0][..100], &vec![2.0; 100]);
        assert!(frames[0][100..].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn reset_discards_carry() {
        let mut framer = Framer::new(512);
        framer.push(&vec![1.0; 100], |_| {});
        framer.reset();

        let new_samples: Vec<f32> = (0..512).map(|sample| (sample + 10) as f32).collect();
        let mut frames = Vec::new();
        framer.push(&new_samples, |frame| frames.push(frame.to_vec()));
        assert_eq!(frames, vec![new_samples]);
    }
}
