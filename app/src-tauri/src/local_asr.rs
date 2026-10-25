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
    gpu: bool,
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

    /// Charge le modèle s'il n'est pas déjà en mémoire avec le même mode (bloquant,
    /// 1 à 3 s pour un grand modèle). `gpu` : tenter le backend Vulkan ; whisper.cpp
    /// retombe sur le CPU s'il n'y a pas de GPU utilisable.
    pub fn ensure_loaded(&self, path: &Path, gpu: bool) -> Result<Arc<WhisperContext>> {
        if let Some(l) = self.loaded.lock().as_ref() {
            if l.path == path && l.gpu == gpu {
                return Ok(l.ctx.clone());
            }
        }
        LOGGING.call_once(whisper_rs::install_logging_hooks);
        let started = Instant::now();
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow!("chemin de modèle non UTF-8 : {}", path.display()))?;
        let mut params = WhisperContextParameters::default();
        params.use_gpu(gpu);
        let ctx = WhisperContext::new_with_params(path_str, params)
            .with_context(|| format!("chargement du modèle {}", path.display()))?;
        log::info!(
            "modèle whisper chargé en {} ms ({}) : {}",
            started.elapsed().as_millis(),
            if gpu { "GPU demandé" } else { "CPU" },
            path.display()
        );
        let ctx = Arc::new(ctx);
        *self.loaded.lock() = Some(Loaded {
            path: path.to_path_buf(),
            gpu,
            ctx: ctx.clone(),
        });
        Ok(ctx)
    }

    pub fn unload(&self) {
        *self.loaded.lock() = None;
    }

    /// Charge le modèle puis exécute une inférence sur du silence : sur GPU, la première
    /// inférence compile les pipelines Vulkan (10 à 20 s) ; autant le faire au démarrage.
    pub fn warm_up(&self, path: &Path, language: &str, threads: u32, gpu: bool) -> Result<()> {
        let silence = vec![0.0f32; MIN_SAMPLES];
        let started = Instant::now();
        let _ = self.transcribe(path, &silence, language, threads, gpu, "", "")?;
        log::info!("préchauffage whisper terminé en {} ms", started.elapsed().as_millis());
        Ok(())
    }

    /// `samples` : PCM f32 mono 16 kHz. `language` : code ISO ou vide pour la détection.
    /// `context` : texte déjà transcrit juste avant (transcription en continu), ou vide.
    /// `terms` : vocabulaire personnel (« Dictae, Overconsulting »), ou vide.
    pub fn transcribe(
        &self,
        path: &Path,
        samples: &[f32],
        language: &str,
        threads: u32,
        gpu: bool,
        context: &str,
        terms: &str,
    ) -> Result<LocalTranscript> {
        let ctx = self.ensure_loaded(path, gpu)?;
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
        // Un amorçage ponctué incite le modèle à ponctuer lui aussi ; en continu, on y
        // ajoute la fin du texte précédent pour garder la cohérence entre segments.
        let prompt = build_prompt(initial_prompt(lang), context, terms);
        if !prompt.is_empty() {
            params.set_initial_prompt(&prompt);
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

/// Amorce = primer de langue + vocabulaire personnel + ~150 derniers caractères du
/// contexte, sans octet nul. Le vocabulaire aide whisper à orthographier les noms propres.
fn build_prompt(primer: Option<&str>, context: &str, terms: &str) -> String {
    let mut prompt = primer.unwrap_or("").to_string();
    let terms = terms.trim().replace('\0', "");
    if !terms.is_empty() {
        if !prompt.is_empty() {
            prompt.push(' ');
        }
        prompt.push_str(&terms);
        prompt.push('.');
    }
    let context = context.trim().replace('\0', "");
    if !context.is_empty() {
        let tail: String = {
            let chars: Vec<char> = context.chars().collect();
            let start = chars.len().saturating_sub(150);
            chars[start..].iter().collect()
        };
        if !prompt.is_empty() {
            prompt.push(' ');
        }
        prompt.push_str(&tail);
    }
    prompt
}

/// Phrases que whisper hallucine sur du silence ou du bruit (sous-titres de YouTube…).
const HALLUCINATIONS: &[&str] = &[
    "sous-titres réalisés",
    "sous-titrage",
    "amara.org",
    "merci d'avoir regardé",
    "thank you for watching",
    "thanks for watching",
    "subtitles by",
    "abonnez-vous",
];

/// Retire les marqueurs non parlés que whisper émet parfois : `[Musique]`, `(rires)`, `♪`…
/// et les phrases hallucinées connues.
fn clean_text(text: &str) -> String {
    let lower = text.to_lowercase();
    if HALLUCINATIONS.iter().any(|h| lower.contains(h)) {
        return String::new();
    }
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
        assert_eq!(clean_text("Sous-titres réalisés par la communauté d'Amara.org"), "");
    }

    #[test]
    fn prompt_keeps_primer_and_context_tail() {
        assert_eq!(build_prompt(None, "", ""), "");
        assert_eq!(build_prompt(Some("Bonjour."), "", ""), "Bonjour.");
        let long = "x".repeat(400);
        let p = build_prompt(Some("Bonjour."), &long, "");
        assert_eq!(p.chars().count(), "Bonjour. ".chars().count() + 150);
        assert_eq!(build_prompt(None, "fin\0 nulle", ""), "fin nulle");
        assert_eq!(build_prompt(Some("Bonjour."), "suite", "Dictae, REA"), "Bonjour. Dictae, REA. suite");
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
        let t = asr.transcribe(&model, &samples, "fr", 0, true, "", "Dictae").unwrap();
        eprintln!("[{}] 1er appel (chargement + pipelines GPU) {} ms → {:?}", model.display(), t.elapsed_ms, t.text);
        let t2 = asr.transcribe(&model, &samples, "fr", 0, true, "", "Dictae").unwrap();
        eprintln!("[{}] 2e appel (chaud) {} ms", model.display(), t2.elapsed_ms);
        let lower = t.text.to_lowercase();
        assert!(lower.contains("bonjour"), "{lower}");
        assert!(lower.contains("demain"), "{lower}");

        // Même fixture en continu : morceaux de 100 ms poussés à vitesse réelle accélérée.
        let asr = std::sync::Arc::new(asr);
        let asr_for_engine = asr.clone();
        let model_for_engine = model.clone();
        let engine = move |seg: &[f32], context: &str| -> anyhow::Result<String> {
            Ok(asr_for_engine.transcribe(&model_for_engine, seg, "fr", 0, true, context, "Dictae")?.text)
        };
        let chunks: Vec<Vec<f32>> = samples.chunks(TARGET_SAMPLE_RATE as usize / 10).map(|c| c.to_vec()).collect();
        let total = chunks.len();
        let mut iter = chunks.into_iter();
        let source = move || iter.next().unwrap_or_default();
        let started = std::time::Instant::now();
        let session = crate::stream::StreamSession::start(source, TARGET_SAMPLE_RATE, engine, |t| eprintln!("partiel : {t}"));
        std::thread::sleep(std::time::Duration::from_millis(100 * total as u64 + 200));
        let streamed = session.stop_and_join().unwrap();
        eprintln!("flux terminé {} ms après le relâchement simulé → {:?}", started.elapsed().as_millis() as u64 - 100 * total as u64 - 200, streamed);
        let lower = streamed.to_lowercase();
        assert!(lower.contains("bonjour"), "{lower}");
        assert!(lower.contains("demain"), "{lower}");
    }
}
