// Device-free format policy; native cpal formats are mapped at the capture boundary.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SampleFormat {
    F32,
    I16,
    I32,
    I24,
    U16,
    I8,
    U8,
    U24,
    U32,
    I64,
    U64,
    F64,
}
#[cfg(test)]
use dasp_sample::{FromSample, Sample};

/// Ranks sample formats by conversion cost, lowest first: `0` is F32 (no
/// conversion), then I16 (what CoreAudio and WASAPI most commonly offer
/// alongside F32), then I32, then I24 (WASAPI's 24-in-32 packing). Every
/// other known format ranks `4` (usable, but rare in practice). The native
/// boundary rejects unknown cpal variants before applying this policy.
pub(crate) fn format_rank(format: SampleFormat) -> Option<u8> {
    match format {
        SampleFormat::F32 => Some(0),
        SampleFormat::I16 => Some(1),
        SampleFormat::I32 => Some(2),
        SampleFormat::I24 => Some(3),
        SampleFormat::U16
        | SampleFormat::I8
        | SampleFormat::U8
        | SampleFormat::U24
        | SampleFormat::U32
        | SampleFormat::I64
        | SampleFormat::U64
        | SampleFormat::F64 => Some(4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_rank_prefers_f32_then_integer_widths() {
        assert_eq!(format_rank(SampleFormat::F32), Some(0));
        assert_eq!(format_rank(SampleFormat::I16), Some(1));
        assert_eq!(format_rank(SampleFormat::I32), Some(2));
        assert_eq!(format_rank(SampleFormat::I24), Some(3));
        assert_eq!(format_rank(SampleFormat::U16), Some(4));
        assert_eq!(format_rank(SampleFormat::F64), Some(4));

        for format in [
            SampleFormat::F32,
            SampleFormat::I16,
            SampleFormat::I32,
            SampleFormat::I24,
            SampleFormat::U16,
            SampleFormat::I8,
            SampleFormat::U8,
            SampleFormat::U24,
            SampleFormat::U32,
            SampleFormat::I64,
            SampleFormat::U64,
            SampleFormat::F64,
        ] {
            assert!(format_rank(format).is_some(), "missing rank for {format:?}");
        }
    }

    #[test]
    fn dasp_conversions_map_full_scale_to_unit_range() {
        assert_eq!(f32::from_sample(i16::MIN), -1.0);
        assert_eq!(f32::from_sample(i16::MAX), 1.0 - 1.0 / 32768.0);
        assert_eq!(f32::from_sample(0u16), -1.0);
        assert_eq!(f32::from_sample(u16::MAX), 1.0 - 1.0 / 32768.0);
        assert_eq!(f32::from_sample(i32::MIN), -1.0);
        assert_eq!(
            f32::from_sample(dasp_sample::I24::new(-(1 << 23)).unwrap()),
            -1.0
        );
        assert_eq!(
            f32::from_sample(dasp_sample::I24::new((1 << 23) - 1).unwrap()),
            1.0 - 1.0 / 8_388_608.0
        );
    }
}
