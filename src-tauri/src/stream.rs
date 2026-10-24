//! Transcription en continu pendant l'enregistrement.
//!
//! Le `Segmenter` découpe l'audio 16 kHz aux pauses (détection d'activité vocale par
//! énergie). Chaque segment part vers un worker qui transcrit séquentiellement, avec le
//! texte précédent comme amorce. Au relâchement, seule la fin reste à transcrire.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;

use crate::audio::{Resampler, TARGET_SAMPLE_RATE};

// ----- segmentation ----------------------------------------------------------

const FRAME: usize = TARGET_SAMPLE_RATE as usize / 50; // 20 ms
const SILENCE_END_FRAMES: usize = 25; // 500 ms de silence clôt un segment
const MIN_SPEECH_FRAMES: usize = 40; // 0,8 s de parole minimum avant de couper sur une pause
const MIN_FLUSH_SPEECH_FRAMES: usize = 8; // 160 ms de parole suffisent pour la fin
const MAX_SEGMENT_FRAMES: usize = 600; // 12 s : coupure forcée au point le plus calme
const FORCED_CUT_SEARCH_FRAMES: usize = 100; // … cherché dans les 2 dernières secondes
const PRE_SPEECH_KEEP_FRAMES: usize = 25; // garder 0,5 s de silence avant la parole
const TAIL_SILENCE_KEEP_FRAMES: usize = 10; // garder 0,2 s de silence après la parole

pub struct Segmenter {
    pending: Vec<f32>,
    /// RMS de chaque trame complète déjà analysée dans `pending`.
    frame_rms: Vec<f32>,
    noise_floor: f32,
    had_speech: bool,
    speech_frames: usize,
    silence_run: usize,
}

impl Default for Segmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Segmenter {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            frame_rms: Vec::new(),
            noise_floor: 0.003,
            had_speech: false,
            speech_frames: 0,
            silence_run: 0,
        }
    }

    fn threshold(&self) -> f32 {
        (self.noise_floor * 3.5).max(0.004)
    }

    fn reset_segment_state(&mut self) {
        self.had_speech = false;
        self.speech_frames = 0;
        self.silence_run = 0;
    }

    /// Ajoute de l'audio 16 kHz ; renvoie les segments prêts à transcrire.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Vec<f32>> {
        self.pending.extend_from_slice(samples);
        let mut ready = Vec::new();
        while self.pending.len() >= (self.frame_rms.len() + 1) * FRAME {
            let start = self.frame_rms.len() * FRAME;
            let frame = &self.pending[start..start + FRAME];
            let rms = (frame.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt();
            self.frame_rms.push(rms);

            let is_speech = rms > self.threshold();
            if rms < self.noise_floor {
                self.noise_floor = self.noise_floor * 0.9 + rms * 0.1;
            } else if !is_speech {
                self.noise_floor = (self.noise_floor * 0.995 + rms * 0.005).min(0.02);
            }

            if !self.had_speech {
                if is_speech {
                    self.had_speech = true;
                    self.speech_frames = 1;
                    self.silence_run = 0;
                } else if self.frame_rms.len() > PRE_SPEECH_KEEP_FRAMES {
                    // Silence initial : ne garder qu'une courte amorce.
                    let drop_frames = self.frame_rms.len() - PRE_SPEECH_KEEP_FRAMES;
                    self.pending.drain(..drop_frames * FRAME);
                    self.frame_rms.drain(..drop_frames);
                }
                continue;
            }

            if is_speech {
                self.speech_frames += 1;
                self.silence_run = 0;
            } else {
                self.silence_run += 1;
            }

            let frames = self.frame_rms.len();
            if self.silence_run >= SILENCE_END_FRAMES && self.speech_frames >= MIN_SPEECH_FRAMES {
                let cut = frames - (self.silence_run - TAIL_SILENCE_KEEP_FRAMES);
                ready.push(self.take_segment(cut));
            } else if frames >= MAX_SEGMENT_FRAMES {
                let search_from = frames - FORCED_CUT_SEARCH_FRAMES;
                let (quietest, _) = self.frame_rms[search_from..]
                    .iter()
                    .enumerate()
                    .fold((0usize, f32::MAX), |best, (i, &v)| if v < best.1 { (i, v) } else { best });
                ready.push(self.take_segment(search_from + quietest + 1));
            }
        }
        ready
    }

    /// Détache `pending[..cut_frames * FRAME]` comme segment et réinitialise l'état.
    fn take_segment(&mut self, cut_frames: usize) -> Vec<f32> {
        let cut = cut_frames.min(self.frame_rms.len()) * FRAME;
        let segment: Vec<f32> = self.pending.drain(..cut).collect();
        self.frame_rms.drain(..cut_frames.min(self.frame_rms.len()));
        self.reset_segment_state();
        segment
    }

    /// Fin d'enregistrement : renvoie le reste s'il contient un peu de parole.
    pub fn flush(&mut self) -> Option<Vec<f32>> {
        let rest = std::mem::take(&mut self.pending);
        self.frame_rms.clear();
        let had = self.had_speech && self.speech_frames >= MIN_FLUSH_SPEECH_FRAMES;
        self.reset_segment_state();
        if had && !rest.is_empty() {
            Some(rest)
        } else {
            None
        }
    }
}

// ----- moteur de transcription par segment --------------------------------------

/// Transcrit un segment 16 kHz ; `context` = texte déjà transcrit (amorce).
pub trait SegmentEngine: Send + 'static {
    fn transcribe_segment(&self, samples: &[f32], context: &str) -> Result<String>;
}

impl<F> SegmentEngine for F
where
    F: Fn(&[f32], &str) -> Result<String> + Send + 'static,
{
    fn transcribe_segment(&self, samples: &[f32], context: &str) -> Result<String> {
        self(samples, context)
    }
}

// ----- session -------------------------------------------------------------------

const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub struct StreamSession {
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<JoinHandle<Result<String>>>>,
}

impl StreamSession {
    /// `source` renvoie les nouveaux échantillons bruts (fréquence `source_rate`, mono)
    /// depuis l'appel précédent. `on_partial` reçoit le texte cumulé après chaque segment.
    pub fn start<S, P>(source: S, source_rate: u32, engine: impl SegmentEngine, on_partial: P) -> Arc<Self>
    where
        S: FnMut() -> Vec<f32> + Send + 'static,
        P: Fn(&str) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let handle = thread::Builder::new()
            .name("dictae-stream".into())
            .spawn(move || run(source, source_rate, engine, on_partial, stop_for_thread))
            .expect("impossible de démarrer le thread de flux");
        Arc::new(Self {
            stop,
            handle: Mutex::new(Some(handle)),
        })
    }

    /// Demande l'arrêt : la fin de l'audio est transcrite, puis le texte complet renvoyé.
    pub fn stop_and_join(&self) -> Result<String> {
        self.stop.store(true, Ordering::SeqCst);
        let handle = self
            .handle
            .lock()
            .take()
            .ok_or_else(|| anyhow!("session de flux déjà consommée"))?;
        handle
            .join()
            .map_err(|_| anyhow!("le thread de flux a paniqué"))?
    }

    /// Abandonne la session sans attendre le résultat.
    pub fn abort(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.lock().take() {
            thread::spawn(move || {
                let _ = handle.join();
            });
        }
    }
}

fn run<S, P>(mut source: S, source_rate: u32, engine: impl SegmentEngine, on_partial: P, stop: Arc<AtomicBool>) -> Result<String>
where
    S: FnMut() -> Vec<f32> + Send + 'static,
    P: Fn(&str) + Send + 'static,
{
    let (seg_tx, seg_rx) = mpsc::channel::<Vec<f32>>();
    let worker = thread::Builder::new()
        .name("dictae-whisper-worker".into())
        .spawn(move || worker_loop(seg_rx, engine, on_partial))
        .map_err(|e| anyhow!("thread worker : {e}"))?;

    let mut resampler = Resampler::new(source_rate, TARGET_SAMPLE_RATE);
    let mut segmenter = Segmenter::new();
    let mut segments_sent = 0usize;
    loop {
        let stopping = stop.load(Ordering::SeqCst);
        let raw = source();
        if !raw.is_empty() {
            let converted = resampler.push(&raw);
            for segment in segmenter.push(&converted) {
                segments_sent += 1;
                let _ = seg_tx.send(segment);
            }
        }
        if stopping {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    if let Some(tail) = segmenter.flush() {
        segments_sent += 1;
        let _ = seg_tx.send(tail);
    }
    drop(seg_tx);
    let texts = worker
        .join()
        .map_err(|_| anyhow!("le worker whisper a paniqué"))?;
    log::info!("flux : {segments_sent} segment(s) transcrit(s)");
    Ok(texts.join(" ").split_whitespace().collect::<Vec<_>>().join(" "))
}

fn worker_loop<P>(rx: mpsc::Receiver<Vec<f32>>, engine: impl SegmentEngine, on_partial: P) -> Vec<String>
where
    P: Fn(&str),
{
    let mut texts: Vec<String> = Vec::new();
    while let Ok(segment) = rx.recv() {
        let context = texts.join(" ");
        match engine.transcribe_segment(&segment, &context) {
            Ok(text) => {
                let text = text.trim().to_string();
                if !text.is_empty() {
                    texts.push(text);
                    on_partial(&texts.join(" "));
                }
            }
            Err(e) => log::warn!("segment ignoré : {e:#}"),
        }
    }
    texts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(len: usize, amplitude: f32, seed: &mut u32) -> Vec<f32> {
        (0..len)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    fn secs(s: f32) -> usize {
        (s * TARGET_SAMPLE_RATE as f32) as usize
    }

    #[test]
    fn segmenter_cuts_on_pauses_and_flushes_the_tail() {
        let mut seed = 42u32;
        let mut audio = Vec::new();
        audio.extend(noise(secs(0.5), 0.001, &mut seed)); // silence initial
        audio.extend(noise(secs(2.0), 0.1, &mut seed)); // parole
        audio.extend(noise(secs(0.8), 0.001, &mut seed)); // pause
        audio.extend(noise(secs(1.5), 0.1, &mut seed)); // parole
        audio.extend(noise(secs(0.3), 0.001, &mut seed)); // fin

        let mut seg = Segmenter::new();
        let mut ready = Vec::new();
        for chunk in audio.chunks(secs(0.1)) {
            ready.extend(seg.push(chunk));
        }
        assert_eq!(ready.len(), 1, "un segment coupé à la pause");
        let first = ready[0].len();
        assert!(first >= secs(2.0) && first <= secs(3.3), "{first}");
        let tail = seg.flush().expect("la fin contient de la parole");
        assert!(tail.len() >= secs(1.5), "{}", tail.len());
        assert!(seg.flush().is_none());
    }

    #[test]
    fn segmenter_ignores_pure_silence() {
        let mut seed = 7u32;
        let mut seg = Segmenter::new();
        let silence = noise(secs(5.0), 0.001, &mut seed);
        assert!(seg.push(&silence).is_empty());
        assert!(seg.flush().is_none());
    }

    #[test]
    fn segmenter_forces_a_cut_on_long_speech() {
        let mut seed = 3u32;
        let mut seg = Segmenter::new();
        let long = noise(secs(14.0), 0.1, &mut seed);
        let ready = seg.push(&long);
        assert_eq!(ready.len(), 1);
        assert!(ready[0].len() >= secs(10.0) && ready[0].len() <= secs(12.0));
    }

    #[test]
    fn session_streams_segments_in_order_with_context() {
        // Source : 3 s de bruit fort toutes les 100 ms, puis une pause, puis 1 s.
        let mut seed = 11u32;
        let mut audio = Vec::new();
        audio.extend(noise(secs(2.0), 0.1, &mut seed));
        audio.extend(noise(secs(0.8), 0.001, &mut seed));
        audio.extend(noise(secs(1.0), 0.1, &mut seed));
        let chunks: Vec<Vec<f32>> = audio.chunks(secs(0.1)).map(|c| c.to_vec()).collect();
        let mut iter = chunks.into_iter();
        let source = move || iter.next().unwrap_or_default();

        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_for_engine = calls.clone();
        let engine = move |samples: &[f32], context: &str| -> Result<String> {
            calls_for_engine.lock().push(context.to_string());
            Ok(format!("seg{}", samples.len() / secs(0.5)))
        };
        let partials = Arc::new(Mutex::new(Vec::<String>::new()));
        let partials_for_cb = partials.clone();
        let session = StreamSession::start(source, TARGET_SAMPLE_RATE, engine, move |t| partials_for_cb.lock().push(t.to_string()));
        thread::sleep(Duration::from_millis(100 * 40 + 300));
        let text = session.stop_and_join().unwrap();

        let calls = calls.lock();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0], "", "premier segment sans contexte");
        assert!(calls[1].starts_with("seg"), "le second reçoit le texte du premier : {:?}", calls[1]);
        assert_eq!(text.split(' ').count(), 2, "{text}");
        assert_eq!(partials.lock().len(), 2);
    }
}
