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
use crate::context::{self, InsertionContext, Verification};
use crate::db::HistoryEntry;
use crate::ducking;
use crate::format::{self, FormatConfig, FormatEndpoint};
use crate::models;
use crate::paste;
use crate::settings::Settings;
use crate::sounds::{self, Sound};
use crate::stream::StreamSession;
use crate::vocab::Vocabulary;
use crate::AppState;

/// Longueur maximale du vocabulaire injecté dans l'amorce whisper.
const TERMS_HINT_CHARS: usize = 160;

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
    /// Lecture du contexte de saisie lancée au début de la dictée.
    context_task: Option<std::thread::JoinHandle<Option<InsertionContext>>>,
    /// Sessions audio coupées pendant l'enregistrement (à rétablir).
    ducked: Option<std::thread::JoinHandle<Vec<String>>>,
    /// Le micro est ouvert pour le test de l'accueil, pas pour une dictée.
    mic_test: bool,
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
                context_task: None,
                ducked: None,
                mic_test: false,
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

    fn vocabulary(&self) -> Vocabulary {
        self.app
            .try_state::<AppState>()
            .and_then(|s| s.db.vocab_list().ok())
            .map(Vocabulary::new)
            .unwrap_or_default()
    }

    // ----- déclencheurs -------------------------------------------------

    pub fn on_shortcut_pressed(self: &Arc<Self>) {
        if self.inner.lock().mic_test {
            // Le raccourci pendant le test du micro le termine simplement.
            self.stop_mic_test();
            return;
        }
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

    /// Test du micro (accueil) : ouvre le micro et diffuse le niveau, sans transcription.
    pub fn start_mic_test(self: &Arc<Self>, device: &str) -> Result<()> {
        {
            let g = self.inner.lock();
            if g.phase != Phase::Idle {
                return Err(anyhow!("une dictée est en cours"));
            }
        }
        self.recorder.start(device)?;
        {
            let mut g = self.inner.lock();
            g.phase = Phase::Recording;
            g.mode = Mode::HandsFree;
            g.message = Some("Test du micro".into());
            g.error = false;
            g.generation += 1;
            g.mic_test = true;
        }
        self.emit_state();
        self.spawn_level_loop();
        Ok(())
    }

    pub fn stop_mic_test(self: &Arc<Self>) {
        let was_test = {
            let mut g = self.inner.lock();
            let t = g.mic_test;
            g.mic_test = false;
            t
        };
        if was_test {
            let _ = self.recorder.stop();
            self.set_state(Phase::Idle, Mode::PushToTalk, None, false);
        }
    }

    /// Clic sur la flow bar ou bouton du hub.
    pub fn toggle(self: &Arc<Self>) {
        if self.inner.lock().mic_test {
            self.stop_mic_test();
            return;
        }
        let phase = self.inner.lock().phase;
        match phase {
            Phase::Idle => self.begin(Mode::HandsFree),
            Phase::Recording => self.finish(),
            Phase::Processing => {}
        }
    }

    pub fn cancel(self: &Arc<Self>) {
        if self.inner.lock().mic_test {
            self.stop_mic_test();
            return;
        }
        let (phase, stream) = {
            let mut g = self.inner.lock();
            g.generation += 1;
            g.pressed_at = None;
            g.context_task = None;
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
        self.restore_others();
        self.set_state(Phase::Idle, Mode::PushToTalk, Some("Annulé"), false);
        self.clear_message_later(Duration::from_millis(1500));
    }

    // ----- transitions --------------------------------------------------

    fn play(&self, sound: Sound) {
        if self.settings().sounds_enabled {
            sounds::play(sound);
        }
    }

    /// Coupe le son des autres applications (en arrière-plan) si le réglage est actif.
    fn duck_others(&self) {
        if !self.settings().mute_others {
            return;
        }
        let task = std::thread::Builder::new()
            .name("dictae-ducking".into())
            .spawn(|| match ducking::mute_others() {
                Ok(ids) => {
                    if !ids.is_empty() {
                        log::info!("{} session(s) audio coupée(s) pendant la dictée", ids.len());
                    }
                    ids
                }
                Err(e) => {
                    log::warn!("coupure du son des autres applications impossible : {e:#}");
                    Vec::new()
                }
            })
            .ok();
        self.inner.lock().ducked = task;
    }

    /// Rétablit le son coupé par `duck_others`, sans bloquer l'appelant.
    fn restore_others(&self) {
        let Some(task) = self.inner.lock().ducked.take() else {
            return;
        };
        std::thread::spawn(move || {
            let ids = task.join().unwrap_or_default();
            if ids.is_empty() {
                return;
            }
            match ducking::restore(&ids) {
                Ok(n) => log::info!("{n} session(s) audio rétablie(s)"),
                Err(e) => log::warn!("rétablissement du son impossible : {e:#}"),
            }
        });
    }

    fn begin(self: &Arc<Self>, mode: Mode) {
        let device = self.settings().input_device;
        match self.recorder.start(&device) {
            Ok(()) => {
                let msg = match mode {
                    Mode::HandsFree => "Mains libres : cliquez ou appuyez pour terminer",
                    Mode::PushToTalk => "Enregistrement…",
                };
                self.play(Sound::Start);
                self.set_state(Phase::Recording, mode, Some(msg), false);
                self.duck_others();
                self.spawn_level_loop();
                self.start_context_capture();
                self.start_stream();
            }
            Err(e) => {
                log::error!("démarrage micro impossible : {e:#}");
                self.play(Sound::Error);
                let msg = format!("Micro indisponible : {e}");
                self.set_state(Phase::Idle, Mode::PushToTalk, Some(&msg), true);
                self.clear_message_later(Duration::from_secs(4));
            }
        }
    }

    /// Lit l'application active et le champ focalisé en arrière-plan (UI Automation).
    fn start_context_capture(self: &Arc<Self>) {
        if !self.settings().context_enabled {
            return;
        }
        let task = std::thread::Builder::new()
            .name("dictae-context".into())
            .spawn(|| match context::capture() {
                Ok(ctx) => {
                    log::info!(
                        "contexte : {} « {} » {} | avant {} car., après {} car., sélection {} car., curseur {}, {} ms",
                        ctx.app,
                        ctx.window_title.chars().take(60).collect::<String>(),
                        ctx.url.as_deref().unwrap_or(""),
                        ctx.before.chars().count(),
                        ctx.after.chars().count(),
                        ctx.selected.chars().count(),
                        if ctx.caret_known { "connu" } else { "inconnu" },
                        ctx.capture_ms
                    );
                    Some(ctx)
                }
                Err(e) => {
                    log::warn!("contexte de saisie illisible : {e:#}");
                    None
                }
            })
            .ok();
        self.inner.lock().context_task = task;
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
        let terms = self.vocabulary().terms_hint(TERMS_HINT_CHARS);
        if settings.uses_local_asr() {
            let state = app.state::<AppState>();
            let path = models::installed_path(&state.models_dir, &settings.local_model)?;
            let language = settings.language.clone();
            let threads = settings.local_threads;
            let gpu = settings.local_gpu;
            Some(Box::new(move |samples: &[f32], context: &str| {
                let state = app.state::<AppState>();
                Ok(state
                    .local_asr
                    .transcribe(&path, samples, &language, threads, gpu, context, &terms)?
                    .text)
            }))
        } else {
            let cfg = AsrConfig {
                base_url: settings.asr_base_url.clone(),
                api_key: settings.asr_api_key.clone(),
                model: settings.asr_model.clone(),
                language: settings.language.clone(),
                prompt: terms,
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
        let (generation, mode, stream, context_task) = {
            let mut g = self.inner.lock();
            if g.phase != Phase::Recording {
                return;
            }
            g.phase = Phase::Processing;
            g.generation += 1;
            g.pressed_at = None;
            (g.generation, g.mode, g.stream.take(), g.context_task.take())
        };
        self.play(Sound::Stop);
        self.set_state(Phase::Processing, mode, Some("Transcription…"), false);
        self.restore_others();

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
            let ctx = match context_task {
                Some(task) => tauri::async_runtime::spawn_blocking(move || task.join().ok().flatten())
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            this.process(capture, generation, stream, ctx).await;
        });
    }

    fn is_current(&self, generation: u64) -> bool {
        self.inner.lock().generation == generation
    }

    /// Transcrit l'enregistrement complet avec le moteur choisi (chemin sans flux).
    async fn transcribe_whole(&self, settings: &Settings, capture: Capture) -> Result<Transcribed> {
        let state = self.app.state::<AppState>();
        let terms = self.vocabulary().terms_hint(TERMS_HINT_CHARS);
        if settings.uses_local_asr() {
            let model_id = settings.local_model.clone();
            let path = models::installed_path(&state.models_dir, &model_id)
                .ok_or_else(|| anyhow!("modèle « {model_id} » non téléchargé : ouvrez Réglages › Transcription"))?;
            let language = settings.language.clone();
            let threads = settings.local_threads;
            let gpu = settings.local_gpu;
            let samples = capture.samples;
            let app = self.app.clone();
            let result = tauri::async_runtime::spawn_blocking(move || {
                let state = app.state::<AppState>();
                state.local_asr.transcribe(&path, &samples, &language, threads, gpu, "", &terms)
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
                prompt: terms,
            };
            let t = asr::transcribe(&state.http, &cfg, capture.wav).await?;
            Ok(Transcribed {
                text: t.text,
                latency_ms: t.elapsed_ms,
                engine: cfg.model,
            })
        }
    }

    /// Point de mise en forme selon les réglages : llama.cpp intégré (démarré si besoin) ou Ollama.
    async fn format_endpoint(&self, settings: &Settings) -> Result<FormatEndpoint> {
        if settings.uses_llama() {
            let state = self.app.state::<AppState>();
            let model_id = settings.llm_model.clone();
            let path = models::installed_path(&state.models_dir, &model_id)
                .ok_or_else(|| anyhow!("modèle de mise en forme « {model_id} » non téléchargé : ouvrez Réglages › Mise en forme"))?;
            let port = state.llama.ensure_running(&path, settings.local_gpu).await?;
            Ok(FormatEndpoint::LlamaServer { port })
        } else {
            Ok(FormatEndpoint::Ollama {
                base_url: settings.ollama_base_url.clone(),
                model: settings.ollama_model.clone(),
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

    async fn process(
        self: Arc<Self>,
        capture: Capture,
        generation: u64,
        stream: Option<Arc<StreamSession>>,
        ctx: Option<InsertionContext>,
    ) {
        let state = self.app.state::<AppState>();
        let settings = state.settings.read().clone();
        // Pas de contexte exploitable dans un champ mot de passe.
        let ctx = ctx.filter(|c| !c.password);

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
            app: ctx.as_ref().map(|c| c.app.clone()).filter(|a| !a.is_empty()),
            window_title: ctx.as_ref().map(|c| c.window_title.clone()).filter(|t| !t.is_empty()),
            url: ctx.as_ref().and_then(|c| c.url.clone()),
        };

        let raw = match transcribed {
            Ok(t) => {
                entry.model = t.engine;
                entry.latency_ms = t.latency_ms as i64;
                t.text.trim().to_string()
            }
            Err(e) => {
                log::error!("transcription impossible : {e:#}");
                self.play(Sound::Error);
                entry.status = "asr_failed".into();
                entry.error = Some(format!("{e:#}"));
                let msg = format!("Erreur de transcription : {e}");
                self.set_state(Phase::Idle, Mode::PushToTalk, Some(&msg), true);
                self.store_and_notify(entry);
                return;
            }
        };
        entry.asr_text = raw.clone();

        // Vocabulaire personnel : remplacements sur le brut, termes pour la mise en forme,
        // snippets après la mise en forme.
        let vocabulary = self.vocabulary();
        let replaced = vocabulary.apply_replacements(&raw);
        if !replaced.used_ids.is_empty() {
            log::info!("{} remplacement(s) du dictionnaire appliqué(s)", replaced.used_ids.len());
            let _ = state.db.vocab_bump(&replaced.used_ids);
        }
        let raw = replaced.text;

        if raw.is_empty() {
            entry.status = "empty".into();
            self.set_state(Phase::Idle, Mode::PushToTalk, Some("Rien entendu"), false);
            self.store_and_notify(entry);
            return;
        }

        // Mise en forme optionnelle par Ollama ; en cas d'échec on colle le texte brut.
        let mut final_text = raw.clone();
        if settings.format_enabled {
            self.set_state(Phase::Processing, Mode::PushToTalk, Some("Mise en forme…"), false);
            let endpoint = self.format_endpoint(&settings).await;
            let formatted = match endpoint {
                Ok(endpoint) => {
                    let cfg = FormatConfig {
                        endpoint,
                        instructions: settings.format_instructions.clone(),
                        context: ctx.clone(),
                        terms: vocabulary.terms_for_prompt(40),
                    };
                    format::format_text(&state.http, &cfg, &raw).await.map(|f| (f, cfg.endpoint.label()))
                }
                Err(e) => Err(e),
            };
            match formatted {
                Ok((f, label)) => {
                    log::info!("mise en forme par {label} en {} ms", f.elapsed_ms);
                    entry.latency_ms += f.elapsed_ms as i64;
                    entry.model = format!("{} + {label}", entry.model);
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

        // Snippets : un déclencheur dicté est remplacé par son texte.
        let expanded = vocabulary.apply_snippets(&final_text);
        if !expanded.used_ids.is_empty() {
            log::info!("{} snippet(s) inséré(s)", expanded.used_ids.len());
            let _ = state.db.vocab_bump(&expanded.used_ids);
            final_text = expanded.text;
            entry.formatted_text = Some(final_text.clone());
        }

        // Espaces et majuscule initiale selon ce qui entoure le curseur.
        if let Some(c) = ctx.as_ref() {
            final_text = context::smart_spacing(&final_text, c);
        }

        let to_paste = final_text.clone();
        let (restore_clipboard, paste_delay_ms) = (settings.restore_clipboard, settings.paste_delay_ms);
        let pasted = tauri::async_runtime::spawn_blocking(move || paste::paste_text(&to_paste, restore_clipboard, paste_delay_ms)).await;
        match pasted {
            Ok(Ok(())) => {
                entry.pasted_text = Some(final_text.clone());
                let verification = match ctx.as_ref() {
                    Some(c) if c.readable => {
                        let text = final_text.clone();
                        tauri::async_runtime::spawn_blocking(move || context::verify_pasted(&text))
                            .await
                            .unwrap_or(Verification::Unavailable)
                    }
                    _ => Verification::Unavailable,
                };
                let msg = match (verification, entry.error.is_some()) {
                    (Verification::Missing, _) => {
                        entry.status = "paste_unverified".into();
                        "Collé ? Le texte n'a pas été retrouvé dans le champ"
                    }
                    (_, true) => "Collé (sans mise en forme)",
                    (Verification::Verified, false) => "Collé ✓ (vérifié)",
                    (Verification::Unavailable, false) => "Collé ✓",
                };
                self.play(if verification == Verification::Missing { Sound::Error } else { Sound::Done });
                self.set_state(Phase::Idle, Mode::PushToTalk, Some(msg), false);
            }
            Ok(Err(e)) => {
                log::error!("collage impossible : {e:#}");
                self.play(Sound::Error);
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
