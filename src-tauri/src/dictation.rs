//! Machine d'états de la dictée : Idle → Recording → Processing → Idle.
//!
//! Déclencheurs : raccourci global (appui / relâchement), clic sur la flow bar,
//! commandes depuis le hub. Les transitions sont diffusées aux fenêtres par événements.
//! Pendant l'enregistrement, une `StreamSession` transcrit les segments au fil de l'eau ;
//! au relâchement il ne reste que la fin à traiter.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::asr::{self, AsrConfig};
use crate::audio::{self, Capture, Recorder};
use crate::db::HistoryEntry;
use crate::format::{self, FormatConfig};
use crate::models;
use crate::paste;
use crate::settings::Settings;
use crate::stream::StreamSession;
use crate::AppState;

pub const EVENT_STATE: &str = "dictation://state";
pub const EVENT_LEVEL: &str = "dictation://level";
pub const EVENT_PARTIAL: &str = "dictation://partial";
pub const EVENT_RESULT: &str = "dictation://result";

const MIN_DURATION_MS: u64 = 300;
const LEVEL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Recording,
    Processing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    PushToTalk,
    HandsFree,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatePayload {
    pub phase: Phase,
    pub mode: Mode,
    pub message: Option<String>,
    pub error: bool,
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LevelPayload {
    level: f32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PartialPayload {
    text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResultPayload {
    entry: HistoryEntry,
}

struct Inner {
    phase: Phase,
    mode: Mode,
    message: Option<String>,
    error: bool,
    /// Incrémenté à chaque fin ou annulation : permet d'ignorer un résultat périmé.
    generation: u64,
    pressed_at: Option<Instant>,
    stream: Option<Arc<StreamSession>>,
}

pub struct Dictation {
    app: AppHandle,
    recorder: Recorder,
    inner: Mutex<Inner>,
}

/// Résultat d'une transcription, quel que soit le moteur.
struct Transcribed {
    text: String,
    latency_ms: u64,
    engine: String,
}

impl Dictation {
    pub fn new(app: AppHandle, recorder: Recorder) -> Arc<Self> {
        Arc::new(Self {
            app,
            recorder,
            inner: Mutex::new(Inner {
                phase: Phase::Idle,
                mode: Mode::PushToTalk,
                message: None,
                error: false,
                generation: 0,
                pressed_at: None,
                stream: None,
            }),
        })
    }

    pub fn snapshot(&self) -> StatePayload {
        let g = self.inner.lock();
        StatePayload {
            phase: g.phase,
            mode: g.mode,
            message: g.message.clone(),
            error: g.error,
            generation: g.generation,
        }
    }

    fn emit_state(&self) {
        let payload = self.snapshot();
        if let Err(e) = self.app.emit(EVENT_STATE, payload) {
            log::warn!("émission de l'état impossible : {e}");
        }
    }

    fn set_state(&self, phase: Phase, mode: Mode, message: Option<&str>, error: bool) {
        {
            let mut g = self.inner.lock();
            g.phase = phase;
            g.mode = mode;
            g.message = message.map(str::to_string);
            g.error = error;
        }
        self.emit_state();
    }

    fn settings(&self) -> Settings {
        self.app
            .try_state::<AppState>()
            .map(|s| s.settings.read().clone())
            .unwrap_or_default()
    }

    // ----- déclencheurs -------------------------------------------------

    pub fn on_shortcut_pressed(self: &Arc<Self>) {
        let (phase, mode) = {
            let g = self.inner.lock();
            (g.phase, g.mode)
        };
        match (phase, mode) {
            (Phase::Idle, _) => {
                self.inner.lock().pressed_at = Some(Instant::now());
                self.begin(Mode::PushToTalk);
            }
            (Phase::Recording, Mode::HandsFree) => self.finish(),
            // Répétition automatique de la touche maintenue, ou traitement en cours : ignorer.
            _ => {}
        }
    }

    pub fn on_shortcut_released(self: &Arc<Self>) {
        let threshold = Duration::from_millis(self.settings().hands_free_tap_ms);
        let switch_to_hands_free = {
            let mut g = self.inner.lock();
            if g.phase != Phase::Recording || g.mode != Mode::PushToTalk {
                return;
            }
            let held = g.pressed_at.take().map(|t| t.elapsed()).unwrap_or(Duration::MAX);
            held < threshold
        };
        if switch_to_hands_free {
            self.set_state(
                Phase::Recording,
                Mode::HandsFree,
                Some("Mains libres : appuyez à nouveau pour terminer"),
                false,
            );
        } else {
            self.finish();
        }
    }

    /// Clic sur la flow bar ou bouton du hub.
    pub fn toggle(self: &Arc<Self>) {
        let phase = self.inner.lock().phase;
        match phase {
            Phase::Idle => self.begin(Mode::HandsFree),
            Phase::Recording => self.finish(),
            Phase::Processing => {}
        }
    }

    pub fn cancel(self: &Arc<Self>) {
        let (phase, stream) = {
            let mut g = self.inner.lock();
            g.generation += 1;
            g.pressed_at = None;
            (g.phase, g.stream.take())
        };
        if let Some(stream) = stream {
            stream.abort();
        }
        if phase == Phase::Recording {
            if let Err(e) = self.recorder.stop() {
                log::warn!("arrêt du micro à l'annulation : {e}");
            }
        }
        self.set_state(Phase::Idle, Mode::PushToTalk, Some("Annulé"), false);
        self.clear_message_later(Duration::from_millis(1500));
    }

    // ----- transitions --------------------------------------------------

    fn begin(self: &Arc<Self>, mode: Mode) {
        match self.recorder.start() {
            Ok(()) => {
                let msg = match mode {
                    Mode::HandsFree => "Mains libres : cliquez ou appuyez pour terminer",
                    Mode::PushToTalk => "Enregistrement…",
                };
                self.set_state(Phase::Recording, mode, Some(msg), false);
                self.spawn_level_loop();
                self.start_stream();
            }
            Err(e) => {
                log::error!("démarrage micro impossible : {e:#}");
                let msg = format!("Micro indisponible : {e}");
                self.set_state(Phase::Idle, Mode::PushToTalk, Some(&msg), true);
                self.clear_message_later(Duration::from_secs(4));
            }
        }
    }

    /// Lance la transcription en continu si elle est activée et possible.
    fn start_stream(self: &Arc<Self>) {
        let settings = self.settings();
        if !settings.streaming_enabled {
            return;
        }
        let Some(live) = self.recorder.live() else {
            return;
        };
        let Some(engine) = self.segment_engine(&settings) else {
            return;
        };
        let mut read_idx = 0usize;
        let source = move || {
            let buf = live.buffer.lock().unwrap();
            if buf.len() > read_idx {
                let chunk = buf[read_idx..].to_vec();
                read_idx = buf.len();
                chunk
            } else {
                Vec::new()
            }
        };
        let app = self.app.clone();
        let on_partial = move |text: &str| {
            let _ = app.emit(EVENT_PARTIAL, PartialPayload { text: text.to_string() });
        };
        let session = StreamSession::start(source, live.sample_rate, engine, on_partial);
        self.inner.lock().stream = Some(session);
    }

    /// Fonction de transcription d'un segment pour le moteur configuré.
    fn segment_engine(&self, settings: &Settings) -> Option<Box<dyn Fn(&[f32], &str) -> Result<String> + Send + 'static>> {
        let app = self.app.clone();
        if settings.uses_local_asr() {
            let state = app.state::<AppState>();
            let path = models::installed_path(&state.models_dir, &settings.local_model)?;
            let language = settings.language.clone();
            let threads = settings.local_threads;
            Some(Box::new(move |samples: &[f32], context: &str| {
                let state = app.state::<AppState>();
                Ok(state.local_asr.transcribe(&path, samples, &language, threads, context)?.text)
            }))
        } else {
            let cfg = AsrConfig {
                base_url: settings.asr_base_url.clone(),
                api_key: settings.asr_api_key.clone(),
                model: settings.asr_model.clone(),
                language: settings.language.clone(),
            };
            Some(Box::new(move |samples: &[f32], _context: &str| {
                let wav = audio::encode_wav(samples, audio::TARGET_SAMPLE_RATE)?;
                let state = app.state::<AppState>();
                let t = tauri::async_runtime::block_on(asr::transcribe(&state.http, &cfg, wav))?;
                Ok(t.text)
            }))
        }
    }

    fn spawn_level_loop(self: &Arc<Self>) {
        let this = self.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(LEVEL_INTERVAL);
            if this.inner.lock().phase != Phase::Recording {
                break;
            }
            let level = this.recorder.level();
            let _ = this.app.emit(EVENT_LEVEL, LevelPayload { level });
        });
    }

    fn finish(self: &Arc<Self>) {
        let (generation, mode, stream) = {
            let mut g = self.inner.lock();
            if g.phase != Phase::Recording {
                return;
            }
            g.phase = Phase::Processing;
            g.generation += 1;
            g.pressed_at = None;
            (g.generation, g.mode, g.stream.take())
        };
        self.set_state(Phase::Processing, mode, Some("Transcription…"), false);

        let capture = match self.recorder.stop() {
            Ok(c) => c,
            Err(e) => {
                log::error!("arrêt micro impossible : {e:#}");
                if let Some(stream) = stream {
                    stream.abort();
                }
                let msg = format!("Erreur micro : {e}");
                self.set_state(Phase::Idle, Mode::PushToTalk, Some(&msg), true);
                self.clear_message_later(Duration::from_secs(4));
                return;
            }
        };
        if capture.duration_ms < MIN_DURATION_MS {
            if let Some(stream) = stream {
                stream.abort();
            }
            self.set_state(Phase::Idle, Mode::PushToTalk, Some("Trop court"), false);
            self.clear_message_later(Duration::from_millis(1500));
            return;
        }

        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            this.process(capture, generation, stream).await;
        });
    }

    fn is_current(&self, generation: u64) -> bool {
        self.inner.lock().generation == generation
    }

    /// Transcrit l'enregistrement complet avec le moteur choisi (chemin sans flux).
    async fn transcribe_whole(&self, settings: &Settings, capture: Capture) -> Result<Transcribed> {
        let state = self.app.state::<AppState>();
        if settings.uses_local_asr() {
            let model_id = settings.local_model.clone();
            let path = models::installed_path(&state.models_dir, &model_id)
                .ok_or_else(|| anyhow!("modèle « {model_id} » non téléchargé : ouvrez Réglages › Transcription"))?;
            let language = settings.language.clone();
            let threads = settings.local_threads;
            let samples = capture.samples;
            let app = self.app.clone();
            let result = tauri::async_runtime::spawn_blocking(move || {
                let state = app.state::<AppState>();
                state.local_asr.transcribe(&path, &samples, &language, threads, "")
            })
            .await
            .map_err(|e| anyhow!("tâche whisper interrompue : {e}"))??;
            Ok(Transcribed {
                text: result.text,
                latency_ms: result.elapsed_ms,
                engine: format!("whisper:{model_id}"),
            })
        } else {
            let cfg = AsrConfig {
                base_url: settings.asr_base_url.clone(),
                api_key: settings.asr_api_key.clone(),
                model: settings.asr_model.clone(),
                language: settings.language.clone(),
            };
            let t = asr::transcribe(&state.http, &cfg, capture.wav).await?;
            Ok(Transcribed {
                text: t.text,
                latency_ms: t.elapsed_ms,
                engine: cfg.model,
            })
        }
    }

    /// Termine le flux (transcription de la fin) ; `None` si aucun flux ou s'il a échoué.
    async fn finish_stream(&self, settings: &Settings, stream: Option<Arc<StreamSession>>) -> Option<Transcribed> {
        let stream = stream?;
        let started = Instant::now();
        let joined = tauri::async_runtime::spawn_blocking(move || stream.stop_and_join()).await;
        match joined {
            Ok(Ok(text)) => {
                let engine = if settings.uses_local_asr() {
                    format!("whisper:{} (flux)", settings.local_model)
                } else {
                    format!("{} (flux)", settings.asr_model)
                };
                Some(Transcribed {
                    text,
                    latency_ms: started.elapsed().as_millis() as u64,
                    engine,
                })
            }
            Ok(Err(e)) => {
                log::warn!("flux en échec, transcription complète à la place : {e:#}");
                None
            }
            Err(e) => {
                log::warn!("flux interrompu, transcription complète à la place : {e}");
                None
            }
        }
    }

    async fn process(self: Arc<Self>, capture: Capture, generation: u64, stream: Option<Arc<StreamSession>>) {
        let state = self.app.state::<AppState>();
        let settings = state.settings.read().clone();

        log::info!(
            "dictée : {} ms d'audio ({} Hz source), moteur {}{}",
            capture.duration_ms,
            capture.source_sample_rate,
            if settings.uses_local_asr() { "local" } else { "api" },
            if stream.is_some() { ", en continu" } else { "" }
        );
        let duration_ms = capture.duration_ms as i64;
        let started = Instant::now();
        let transcribed = match self.finish_stream(&settings, stream).await {
            Some(t) => Ok(t),
            None => self.transcribe_whole(&settings, capture).await,
        };
        log::info!("texte disponible {} ms après le relâchement", started.elapsed().as_millis());

        if !self.is_current(generation) {
            log::info!("résultat ignoré : dictée annulée entre-temps");
            self.set_state(Phase::Idle, Mode::PushToTalk, None, false);
            return;
        }

        let mut entry = HistoryEntry {
            id: uuid::Uuid::new_v4().to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            asr_text: String::new(),
            formatted_text: None,
            pasted_text: None,
            status: "ok".into(),
            duration_ms,
            latency_ms: started.elapsed().as_millis() as i64,
            model: String::new(),
            error: None,
        };

        let raw = match transcribed {
            Ok(t) => {
                entry.model = t.engine;
                entry.latency_ms = t.latency_ms as i64;
                t.text.trim().to_string()
            }
            Err(e) => {
                log::error!("transcription impossible : {e:#}");
                entry.status = "asr_failed".into();
                entry.error = Some(format!("{e:#}"));
                let msg = format!("Erreur de transcription : {e}");
                self.set_state(Phase::Idle, Mode::PushToTalk, Some(&msg), true);
                self.store_and_notify(entry);
                return;
            }
        };
        entry.asr_text = raw.clone();

        if raw.is_empty() {
            entry.status = "empty".into();
            self.set_state(Phase::Idle, Mode::PushToTalk, Some("Rien entendu"), false);
            self.store_and_notify(entry);
            return;
        }

        // Mise en forme optionnelle par Ollama ; en cas d'échec on colle le texte brut.
        let mut final_text = raw.clone();
        if settings.format_enabled && !settings.ollama_model.trim().is_empty() {
            self.set_state(Phase::Processing, Mode::PushToTalk, Some("Mise en forme…"), false);
            let cfg = FormatConfig {
                base_url: settings.ollama_base_url.clone(),
                model: settings.ollama_model.clone(),
                instructions: settings.format_instructions.clone(),
            };
            match format::format_text(&state.http, &cfg, &raw).await {
                Ok(f) => {
                    log::info!("mise en forme par {} en {} ms", cfg.model, f.elapsed_ms);
                    entry.latency_ms += f.elapsed_ms as i64;
                    entry.model = format!("{} + {}", entry.model, cfg.model);
                    if f.text != raw {
                        entry.formatted_text = Some(f.text.clone());
                        final_text = f.text;
                    }
                }
                Err(e) => {
                    log::warn!("mise en forme indisponible, texte brut collé : {e:#}");
                    entry.error = Some(format!("mise en forme indisponible : {e:#}"));
                }
            }
            if !self.is_current(generation) {
                self.set_state(Phase::Idle, Mode::PushToTalk, None, false);
                return;
            }
        }

        let to_paste = final_text.clone();
        let (restore_clipboard, paste_delay_ms) = (settings.restore_clipboard, settings.paste_delay_ms);
        let pasted = tauri::async_runtime::spawn_blocking(move || paste::paste_text(&to_paste, restore_clipboard, paste_delay_ms)).await;
        match pasted {
            Ok(Ok(())) => {
                entry.pasted_text = Some(final_text);
                let msg = if entry.error.is_some() { "Collé (sans mise en forme)" } else { "Collé ✓" };
                self.set_state(Phase::Idle, Mode::PushToTalk, Some(msg), false);
            }
            Ok(Err(e)) => {
                log::error!("collage impossible : {e:#}");
                entry.status = "paste_failed".into();
                entry.error = Some(format!("{e:#}"));
                self.set_state(
                    Phase::Idle,
                    Mode::PushToTalk,
                    Some("Collage impossible : texte laissé dans le presse-papiers"),
                    true,
                );
            }
            Err(e) => {
                entry.status = "paste_failed".into();
                entry.error = Some(format!("tâche de collage interrompue : {e}"));
                self.set_state(Phase::Idle, Mode::PushToTalk, Some("Collage interrompu"), true);
            }
        }
        self.store_and_notify(entry);
    }

    fn store_and_notify(self: &Arc<Self>, entry: HistoryEntry) {
        let state = self.app.state::<AppState>();
        if let Err(e) = state.db.insert(&entry) {
            log::error!("écriture historique impossible : {e:#}");
        }
        let _ = self.app.emit(EVENT_RESULT, ResultPayload { entry });
        self.clear_message_later(Duration::from_millis(3500));
    }

    /// Efface le message d'état après `delay`, sauf si l'état a changé entre-temps.
    fn clear_message_later(self: &Arc<Self>, delay: Duration) {
        let this = self.clone();
        let generation = self.inner.lock().generation;
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let should_clear = {
                let g = this.inner.lock();
                g.phase == Phase::Idle && g.generation == generation && g.message.is_some()
            };
            if should_clear {
                this.set_state(Phase::Idle, Mode::PushToTalk, None, false);
            }
        });
    }
}
