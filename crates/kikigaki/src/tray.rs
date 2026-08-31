#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use kikigaki_core::engine::EnginePhase;
use kikigaki_core::session::State;

const SIZE: usize = 22;
const IDLE: [u8; 4] = [0x8a, 0x8f, 0xa0, 0xff];
const RECORDING: [u8; 4] = [0xe0, 0x52, 0x3b, 0xff];
const FINALIZING: [u8; 4] = [0xc9, 0x8a, 0x12, 0xff];
const LOADING: [u8; 4] = [0xd4, 0x91, 0x18, 0xff];
const DISCONNECTED: [u8; 4] = [0x7a, 0x2e, 0x22, 0xff];
const SLASH: [u8; 4] = [0xf4, 0xe7, 0xe5, 0xff];

pub fn icon_rgba(state: State, phase: EnginePhase) -> Vec<u8> {
    let loading = state == State::Disconnected && phase == EnginePhase::Starting;
    let color = match state {
        State::Idle => IDLE,
        State::Recording => RECORDING,
        State::Finalizing => FINALIZING,
        State::Disconnected if loading => LOADING,
        State::Disconnected => DISCONNECTED,
    };
    let mut rgba = vec![0; SIZE * SIZE * 4];
    let center = (SIZE as f32 - 1.0) / 2.0;
    let radius_squared = 9.5_f32.powi(2);

    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            if dx * dx + dy * dy <= radius_squared {
                let pixel = if state == State::Disconnected && !loading && (x == y || x + 1 == y) {
                    SLASH
                } else {
                    color
                };
                let offset = (y * SIZE + x) * 4;
                rgba[offset..offset + 4].copy_from_slice(&pixel);
            }
        }
    }
    rgba
}

pub fn tooltip(state: State, phase: EnginePhase) -> &'static str {
    match state {
        State::Idle => "kikigaki — idle",
        State::Recording => "kikigaki — recording",
        State::Finalizing => "kikigaki — finalizing",
        State::Disconnected if phase == EnginePhase::Starting => "kikigaki — loading",
        State::Disconnected => "kikigaki — disconnected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(image: &[u8], x: usize, y: usize) -> [u8; 4] {
        let offset = (y * SIZE + x) * 4;
        image[offset..offset + 4].try_into().unwrap()
    }

    #[test]
    fn state_maps_to_tooltip_text() {
        assert_eq!(tooltip(State::Idle, EnginePhase::Ready), "kikigaki — idle");
        assert_eq!(
            tooltip(State::Recording, EnginePhase::Ready),
            "kikigaki — recording"
        );
        assert_eq!(
            tooltip(State::Finalizing, EnginePhase::Ready),
            "kikigaki — finalizing"
        );
        assert_eq!(
            tooltip(State::Disconnected, EnginePhase::Failed),
            "kikigaki — disconnected"
        );
        assert_eq!(
            tooltip(State::Disconnected, EnginePhase::Starting),
            "kikigaki — loading"
        );
    }

    #[test]
    fn state_maps_to_22_pixel_circle_colors() {
        for (state, color) in [
            (State::Idle, IDLE),
            (State::Recording, RECORDING),
            (State::Finalizing, FINALIZING),
        ] {
            let image = icon_rgba(state, EnginePhase::Ready);
            assert_eq!(image.len(), SIZE * SIZE * 4);
            assert_eq!(pixel(&image, 10, 10), color);
            assert_eq!(pixel(&image, 0, 0), [0, 0, 0, 0]);
        }
    }

    #[test]
    fn disconnected_icon_has_two_pixel_diagonal_slash() {
        let image = icon_rgba(State::Disconnected, EnginePhase::Failed);
        assert_eq!(pixel(&image, 10, 10), SLASH);
        assert_eq!(pixel(&image, 10, 11), SLASH);
        assert_eq!(pixel(&image, 11, 10), DISCONNECTED);
        assert_eq!(pixel(&image, 15, 10), DISCONNECTED);
    }

    #[test]
    fn disconnected_starting_uses_loading_color_without_slash() {
        let image = icon_rgba(State::Disconnected, EnginePhase::Starting);
        assert_eq!(pixel(&image, 10, 10), LOADING);
        assert_eq!(pixel(&image, 10, 11), LOADING);
    }
}
