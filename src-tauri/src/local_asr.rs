//! Transcription locale avec whisper.cpp (crate `whisper-rs`), entièrement hors ligne.
//!
//! Le contexte (modèle chargé en mémoire) est conservé entre deux dictées ;
//! un `WhisperState` est créé à chaque transcription.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::audio::TARGET_SAMPLE_RATE;

/// whisper.cpp refuse les entrées plus courtes qu'une seconde : on complète par du silence.
const MIN_SAMPLES: usize = (TARGET_SAMPLE_RATE as usize * 11) / 10;

static LOGGING: Once = Once::new();

struct Loaded {
    path: PathBuf,
    ctx: Arc<WhisperContext>,
}

pub struct LocalAsr {
    loaded: Mutex<Option<Loaded>>,
}

#[derive(Debug)]
pub struct LocalTranscript {
    pub text: String,
    pub elapsed_ms: u64,
}

impl LocalAsr {
    pub fn new() -> Self {
        Self {
            loaded: Mutex::new(None),
        }
    }

    pub fn loaded_path(&self) -> Option<PathBuf> {
        self.loaded.lock().as_ref().map(|l| l.path.clone())
    }

    /// Charge le modèle s'il n'est pas déjà en mémoire (bloquant, 1 à 3 s pour un grand modèle).
    pub fn ensure_loaded(&self, path: &Path) -> Result<Arc<WhisperContext>> {
        if let Some(l) = self.loaded.lock().as_ref() {
            if l.path == path {
                return Ok(l.ctx.clone());
            }
        }
        LOGGING.call_once(whisper_rs::install_logging_hooks);
        let started = Instant::now();
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow!("chemin de modèle non UTF-8 : {}", path.display()))?;
        let mut params = WhisperContextParameters::default();
        params.use_gpu(true);
        let ctx = WhisperContext::new_with_params(path_str, params)
            .with_context(|| format!("chargement du modèle {}", path.display()))?;
        log::info!(
            "modèle whisper chargé en {} ms : {}",
            started.elapsed().as_millis(),
            path.display()
        );
        let ctx = Arc::new(ctx);
        *self.loaded.lock() = Some(Loaded {
            path: path.to_path_buf(),
            ctx: ctx.clone(),
        });
        Ok(ctx)
    }

    pub fn unload(&self) {
        *self.loaded.lock() = None;
    }

    /// `samples` : PCM f32 mono 16 kHz. `language` : code ISO ou vide pour la détection.
    pub fn transcribe(&self, path: &Path, samples: &[f32], language: &str, threads: u32) -> Result<LocalTranscript> {
        let ctx = self.ensure_loaded(path)?;
        let started = Instant::now();

        let mut audio = samples.to_vec();
        if audio.len() < MIN_SAMPLES {
            audio.resize(MIN_SAMPLES, 0.0);
        }

        let language = language.trim().to_ascii_lowercase();
        let lang: &str = if language.is_empty() { "auto" } else { &language };
        let threads = if threads == 0 { default_threads() } else { threads };

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(lang));
        params.set_translate(false);
        params.set_n_threads(threads as i32);
        params.set_no_timestamps(true);
        params.set_suppress_blank(true);
        params.set_single_segment(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        // Un amorçage ponctué incite le modèle à ponctuer lui aussi.
        if let Some(prompt) = initial_prompt(lang) {
            params.set_initial_prompt(prompt);
        }

        let mut state = ctx.create_state().context("création de l'état whisper")?;
        state
            .full(params, &audio)
            .map_err(|e| anyhow!("inférence whisper : {e:?}"))?;

        let mut text = String::new();
        for segment in state.as_iter() {
            let Ok(piece) = segment.to_str_lossy() else {
                continue;
            };
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(piece);
        }

        let elapsed_ms = started.elapsed().as_millis() as u64;
        log::info!(
            "whisper : {:.1} s d'audio transcrits en {} ms ({} threads)",
            samples.len() as f32 / TARGET_SAMPLE_RATE as f32,
            elapsed_ms,
            threads
        );
        Ok(LocalTranscript {
            text: clean_text(&text),
            elapsed_ms,
        })
    }
}

fn default_threads() -> u32 {
    let logical = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) as u32;
    // ggml tourne mieux sur les cœurs physiques (hyperthreading ignoré).
    (logical / 2).clamp(2, 12)
}

fn initial_prompt(lang: &str) -> Option<&'static str> {
    match lang {
        "fr" => Some("Bonjour, voici une dictée en français, avec une ponctuation soignée."),
        "en" => Some("Hello, this is a dictation in English, with careful punctuation."),
        "es" => Some("Hola, este es un dictado en español, con puntuación cuidada."),
        "de" => Some("Hallo, dies ist ein Diktat auf Deutsch, mit sorgfältiger Zeichensetzung."),
        "it" => Some("Ciao, questa è una dettatura in italiano, con punteggiatura accurata."),
        "pt" => Some("Olá, este é um ditado em português, com pontuação cuidada."),
        _ => None,
    }
}

/// Retire les marqueurs non parlés que whisper émet parfois : `[Musique]`, `(rires)`, `♪`…
fn clean_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth_square = 0usize;
    let mut depth_round = 0usize;
    for c in text.chars() {
        match c {
            '[' => depth_square += 1,
            ']' => depth_square = depth_square.saturating_sub(1),
            '(' => depth_round += 1,
            ')' => depth_round = depth_round.saturating_sub(1),
            '♪' | '♫' => {}
            _ if depth_square == 0 && depth_round == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_removes_bracketed_markers() {
        assert_eq!(clean_text(" Bonjour [Musique] le  monde (rires) ♪ "), "Bonjour le monde");
        assert_eq!(clean_text("Rien à retirer."), "Rien à retirer.");
    }

    /// Test réel sur la fixture TTS, sauté si aucun modèle n'est téléchargé.
    #[test]
    fn transcribes_french_fixture_when_a_model_is_installed() {
        let models_dir = std::env::var("APPDATA")
            .map(|a| PathBuf::from(a).join("net.overconsulting.dictae").join("models"))
            .unwrap_or_default();
        // DICTAE_TEST_MODEL=ggml-large-v3-turbo-q5_0.bin pour forcer un modèle précis.
        let forced = std::env::var("DICTAE_TEST_MODEL").ok();
        let mut candidates = vec!["ggml-small-q5_1.bin", "ggml-base-q5_1.bin", "ggml-large-v3-turbo-q5_0.bin"];
        if let Some(f) = forced.as_deref() {
            candidates.insert(0, f);
        }
        let Some(model) = candidates.iter().map(|f| models_dir.join(f)).find(|p| p.is_file()) else {
            eprintln!("aucun modèle whisper dans {}, test sauté", models_dir.display());
            return;
        };
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dictee-fr.wav");
        let mut reader = hound::WavReader::open(&fixture).unwrap();
        assert_eq!(reader.spec().sample_rate, TARGET_SAMPLE_RATE);
        let samples: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / i16::MAX as f32)
            .collect();

        let asr = LocalAsr::new();
        let t = asr.transcribe(&model, &samples, "fr", 0).unwrap();
        eprintln!("[{}] {} ms → {:?}", model.display(), t.elapsed_ms, t.text);
        let lower = t.text.to_lowercase();
        assert!(lower.contains("bonjour"), "{lower}");
        assert!(lower.contains("demain"), "{lower}");
    }
}
