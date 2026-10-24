//! Petits sons de retour (début, fin, collage, erreur), synthétisés au premier usage
//! et joués de manière asynchrone via `PlaySound` (Windows).

use std::sync::OnceLock;

use crate::audio::encode_wav;

#[derive(Debug, Clone, Copy)]
pub enum Sound {
    Start,
    Stop,
    Done,
    Error,
}

const RATE: u32 = 16_000;

/// Enchaînement de notes (fréquence Hz, durée ms) avec enveloppe douce.
fn synth(notes: &[(f32, u32)], gain: f32) -> Vec<u8> {
    let mut samples = Vec::new();
    for &(freq, ms) in notes {
        let n = (RATE * ms / 1000) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let env = {
                let a = (i as f32 / (n as f32 * 0.15)).min(1.0);
                let r = ((n - i) as f32 / (n as f32 * 0.35)).min(1.0);
                a.min(r)
            };
            samples.push((t * freq * std::f32::consts::TAU).sin() * env * gain);
        }
        // courte pause entre les notes
        samples.extend(std::iter::repeat(0.0).take((RATE / 80) as usize));
    }
    encode_wav(&samples, RATE).unwrap_or_default()
}

fn wav(sound: Sound) -> &'static [u8] {
    static START: OnceLock<Vec<u8>> = OnceLock::new();
    static STOP: OnceLock<Vec<u8>> = OnceLock::new();
    static DONE: OnceLock<Vec<u8>> = OnceLock::new();
    static ERROR: OnceLock<Vec<u8>> = OnceLock::new();
    match sound {
        Sound::Start => START.get_or_init(|| synth(&[(660.0, 55), (880.0, 70)], 0.18)),
        Sound::Stop => STOP.get_or_init(|| synth(&[(660.0, 70)], 0.16)),
        Sound::Done => DONE.get_or_init(|| synth(&[(880.0, 45), (1174.0, 80)], 0.14)),
        Sound::Error => ERROR.get_or_init(|| synth(&[(220.0, 160)], 0.2)),
    }
}

#[cfg(windows)]
pub fn play(sound: Sound) {
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
    let data = wav(sound);
    if data.is_empty() {
        return;
    }
    // Les tampons sont statiques : PlaySound peut les lire après le retour de l'appel.
    unsafe {
        let _ = PlaySoundW(PCWSTR(data.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

#[cfg(not(windows))]
pub fn play(_sound: Sound) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sounds_are_short_valid_wavs() {
        for s in [Sound::Start, Sound::Stop, Sound::Done, Sound::Error] {
            let w = wav(s);
            assert_eq!(&w[0..4], b"RIFF");
            let samples = (w.len() - 44) / 2;
            assert!(samples > RATE as usize / 20 && samples < RATE as usize / 2, "{samples}");
        }
    }
}
