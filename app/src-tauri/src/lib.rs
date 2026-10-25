//! Dictae : dictée vocale système. Cœur Tauri 2 / Rust.

mod asr;
mod audio;
mod context;
mod db;
mod dictation;
mod ducking;
mod format;
mod llama;
mod local_asr;
mod models;
mod paste;
mod procguard;
mod settings;
mod sounds;
mod stream;
mod vocab;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::{Mutex, RwLock};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, State, WindowEvent};
use serde::Serialize;
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_updater::UpdaterExt as _;

/// Argument passé par le lancement automatique : démarrer réduit dans la zone de notification.
const HIDDEN_ARG: &str = "--hidden";

use crate::audio::Recorder;
use crate::db::{Db, HistoryEntry};
use crate::dictation::{Dictation, StatePayload};
use crate::local_asr::LocalAsr;
use crate::models::ModelStatus;
use crate::settings::Settings;
use crate::vocab::VocabEntry;

pub struct AppState {
    pub settings: RwLock<Settings>,
    pub settings_path: PathBuf,
    pub models_dir: PathBuf,
    pub db: Db,
    pub http: reqwest::Client,
    pub local_asr: LocalAsr,
    /// Serveur llama.cpp intégré pour la mise en forme.
    pub llama: llama::LlamaServer,
    /// Identifiants des modèles en cours de téléchargement.
    pub downloading: Mutex<Vec<String>>,
    /// Raccourci de réécriture enregistré (pour le distinguer du push-to-talk dans le gestionnaire).
    pub polish_shortcut: Mutex<Option<Shortcut>>,
}

// ----- commandes exposées au frontend ---------------------------------------

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Settings {
    state.settings.read().clone()
}

#[tauri::command]
fn save_settings(app: AppHandle, state: State<'_, AppState>, settings: Settings) -> Result<(), String> {
    let previous = state.settings.read().clone();
    if settings.shortcut != previous.shortcut || settings.polish_shortcut != previous.polish_shortcut {
        if let Err(e) = register_shortcuts(&app, &settings.shortcut, &settings.polish_shortcut) {
            // Remettre les anciens raccourcis pour ne pas laisser l'app sans déclencheur.
            let _ = register_shortcuts(&app, &previous.shortcut, &previous.polish_shortcut);
            return Err(format!("{e:#}"));
        }
    }
    if settings.launch_at_login != previous.launch_at_login {
        apply_autostart(&app, settings.launch_at_login).map_err(|e| format!("{e:#}"))?;
    }
    settings
        .save(&state.settings_path)
        .map_err(|e| format!("{e:#}"))?;
    *state.settings.write() = settings.clone();
    if settings.uses_local_asr()
        && (settings.local_model != previous.local_model || settings.local_gpu != previous.local_gpu || !previous.uses_local_asr())
    {
        preload_local_model(&app);
    }
    if settings.format_enabled != previous.format_enabled
        || settings.format_provider != previous.format_provider
        || settings.llm_model != previous.llm_model
        || settings.local_gpu != previous.local_gpu
    {
        prestart_llama(&app);
    }
    let _ = app.emit("settings://changed", settings);
    Ok(())
}

#[tauri::command]
async fn test_asr(state: State<'_, AppState>) -> Result<String, String> {
    let cfg = {
        let s = state.settings.read();
        asr::AsrConfig {
            base_url: s.asr_base_url.clone(),
            api_key: s.asr_api_key.clone(),
            model: s.asr_model.clone(),
            language: s.language.clone(),
            prompt: String::new(),
        }
    };
    asr::probe(&state.http, &cfg).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn list_input_devices() -> Vec<String> {
    audio::list_input_devices()
}

#[tauri::command]
async fn llama_status(state: State<'_, AppState>) -> Result<llama::LlamaStatus, String> {
    Ok(state.llama.status().await)
}

#[tauri::command]
fn start_mic_test(state: State<'_, AppState>, dictation: State<'_, Arc<Dictation>>) -> Result<(), String> {
    let device = state.settings.read().input_device.clone();
    dictation.start_mic_test(&device).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn stop_mic_test(dictation: State<'_, Arc<Dictation>>) {
    dictation.stop_mic_test();
}

/// Marque l'accueil comme terminé (ou le rouvre avec `completed = false`).
#[tauri::command]
fn set_onboarding_completed(app: AppHandle, state: State<'_, AppState>, completed: bool) -> Result<Settings, String> {
    let mut settings = state.settings.read().clone();
    settings.onboarding_completed = completed;
    settings.save(&state.settings_path).map_err(|e| format!("{e:#}"))?;
    *state.settings.write() = settings.clone();
    let _ = app.emit("settings://changed", settings.clone());
    Ok(settings)
}

/// Les anciens utilisateurs qui ont déjà leurs modèles n'ont pas besoin de l'accueil.
fn autocomplete_onboarding(state: &AppState) {
    let mut settings = state.settings.write();
    if settings.onboarding_completed {
        return;
    }
    let whisper_ok = models::installed_path(&state.models_dir, &settings.local_model).is_some();
    let llm_ok = !settings.format_enabled
        || !settings.uses_llama()
        || models::installed_path(&state.models_dir, &settings.llm_model).is_some();
    if whisper_ok && llm_ok {
        settings.onboarding_completed = true;
        if let Err(e) = settings.save(&state.settings_path) {
            log::warn!("enregistrement des réglages : {e:#}");
        }
        log::info!("accueil considéré comme terminé : modèles déjà présents");
    }
}

/// Démarre le serveur llama.cpp en arrière-plan si la mise en forme intégrée est active
/// et son modèle présent, pour que la première dictée n'attende pas le chargement.
fn prestart_llama(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let (enabled, id, gpu) = {
            let s = state.settings.read();
            (s.format_enabled && s.uses_llama(), s.llm_model.clone(), s.local_gpu)
        };
        if !enabled {
            state.llama.stop().await;
            return;
        }
        match models::installed_path(&state.models_dir, &id) {
            Some(path) => match state.llama.ensure_running(&path, gpu).await {
                Ok(port) => {
                    if let Err(e) = state.llama.warm_up(port).await {
                        log::warn!("{e:#}");
                    }
                }
                Err(e) => log::error!("démarrage de llama-server impossible : {e:#}"),
            },
            None => log::warn!("modèle de mise en forme « {id} » absent : à télécharger dans Réglages"),
        }
    });
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub exe_path: String,
    pub data_dir: String,
    pub log_path: String,
    /// Backends ggml compilés dans ce binaire (whisper.cpp).
    pub whisper_backends: Vec<String>,
    /// Modèle whisper chargé en mémoire, s'il y en a un.
    pub loaded_model: Option<String>,
}

#[tauri::command]
fn get_app_info(app: AppHandle, state: State<'_, AppState>) -> AppInfo {
    let log_path = app
        .path()
        .app_log_dir()
        .map(|d| d.join("Dictae.log").display().to_string())
        .unwrap_or_default();
    AppInfo {
        version: app.package_info().version.to_string(),
        exe_path: std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
        data_dir: state.settings_path.parent().map(|p| p.display().to_string()).unwrap_or_default(),
        log_path,
        whisper_backends: vec![
            "CPU".into(),
            #[cfg(feature = "gpu-vulkan")]
            "Vulkan".into(),
        ],
        loaded_model: state
            .local_asr
            .loaded_path()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned())),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub current_version: String,
    pub available: Option<String>,
    pub notes: Option<String>,
}

/// Interroge le point de mise à jour (GitHub Releases) sans rien installer.
#[tauri::command]
async fn check_update(app: AppHandle) -> Result<UpdateInfo, String> {
    let current_version = app.package_info().version.to_string();
    let updater = app.updater().map_err(|e| format!("updater : {e}"))?;
    let update = updater.check().await.map_err(|e| format!("vérification impossible : {e}"))?;
    Ok(UpdateInfo {
        current_version,
        available: update.as_ref().map(|u| u.version.clone()),
        notes: update.and_then(|u| u.body),
    })
}

/// Télécharge et installe la mise à jour disponible, puis redémarre Dictae.
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| format!("updater : {e}"))?;
    let Some(update) = updater.check().await.map_err(|e| format!("vérification impossible : {e}"))? else {
        return Err("aucune mise à jour disponible".into());
    };
    log::info!("installation de la mise à jour {}", update.version);
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| format!("installation impossible : {e}"))?;
    app.restart();
}

#[tauri::command]
fn list_vocabulary(state: State<'_, AppState>) -> Result<Vec<VocabEntry>, String> {
    state.db.vocab_list().map_err(|e| format!("{e:#}"))
}

/// Crée (id vide) ou met à jour une entrée de vocabulaire ; renvoie l'entrée enregistrée.
#[tauri::command]
fn save_vocabulary(state: State<'_, AppState>, mut entry: VocabEntry) -> Result<VocabEntry, String> {
    let now = chrono::Utc::now().to_rfc3339();
    if entry.id.trim().is_empty() {
        entry.id = uuid::Uuid::new_v4().to_string();
        entry.created_at = now.clone();
        entry.uses = 0;
    }
    entry.updated_at = now;
    entry.phrase = entry.phrase.trim().to_string();
    entry.note = entry.note.trim().to_string();
    if entry.kind == vocab::VocabKind::Term {
        entry.target = String::new();
    }
    state.db.vocab_upsert(&entry).map_err(|e| format!("{e:#}"))?;
    Ok(entry)
}

#[tauri::command]
fn delete_vocabulary(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.db.vocab_delete(&id).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn list_local_models(state: State<'_, AppState>) -> Vec<ModelStatus> {
    let downloading = state.downloading.lock().clone();
    models::statuses(&state.models_dir, &downloading)
}

#[tauri::command]
fn download_local_model(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    let info = models::find(&id).ok_or_else(|| format!("modèle inconnu : {id}"))?;
    {
        let mut downloading = state.downloading.lock();
        if downloading.iter().any(|d| d == &id) {
            return Err("téléchargement déjà en cours".into());
        }
        downloading.push(id.clone());
    }
    let app_for_task = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app_for_task.state::<AppState>();
        let result = models::download(&app_for_task, &state.http, &state.models_dir, info).await;
        state.downloading.lock().retain(|d| d != &id);
        match result {
            Ok(path) => {
                log::info!("modèle téléchargé : {}", path.display());
                let (whisper_id, llm_id) = {
                    let s = state.settings.read();
                    (s.local_model.clone(), s.llm_model.clone())
                };
                if whisper_id == id {
                    preload_local_model(&app_for_task);
                }
                if llm_id == id {
                    prestart_llama(&app_for_task);
                }
            }
            Err(e) => log::error!("téléchargement de {id} impossible : {e:#}"),
        }
    });
    Ok(())
}

#[tauri::command]
fn delete_local_model(state: State<'_, AppState>, id: String) -> Result<(), String> {
    if state.local_asr.loaded_path() == models::find(&id).map(|m| models::file_path(&state.models_dir, m)) {
        state.local_asr.unload();
    }
    models::delete(&state.models_dir, &id).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn list_ollama_models(state: State<'_, AppState>, base_url: Option<String>) -> Result<Vec<String>, String> {
    let base = base_url.unwrap_or_else(|| state.settings.read().ollama_base_url.clone());
    format::list_models(&state.http, &base)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Passe un exemple dans le modèle de mise en forme et renvoie le résultat.
#[tauri::command]
async fn test_format(state: State<'_, AppState>, sample: Option<String>) -> Result<String, String> {
    let settings = state.settings.read().clone();
    let endpoint = if settings.uses_llama() {
        let path = models::installed_path(&state.models_dir, &settings.llm_model)
            .ok_or_else(|| format!("modèle « {} » non téléchargé", settings.llm_model))?;
        let port = state
            .llama
            .ensure_running(&path, settings.local_gpu)
            .await
            .map_err(|e| format!("{e:#}"))?;
        format::FormatEndpoint::LlamaServer { port }
    } else {
        format::FormatEndpoint::Ollama {
            base_url: settings.ollama_base_url.clone(),
            model: settings.ollama_model.clone(),
        }
    };
    let cfg = format::FormatConfig {
        endpoint,
        instructions: settings.format_instructions.clone(),
        context: None,
        terms: state.db.vocab_list().map(|v| vocab::Vocabulary::new(v).terms_for_prompt(40)).unwrap_or_default(),
    };
    let raw = sample.unwrap_or_else(|| {
        "bonjour euh je voulais savoir si on pouvait se voir demain à quatorze heures pour parler du projet merci".into()
    });
    let f = format::format_text(&state.http, &cfg, &raw)
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(format!("{} ({} ms)", f.text, f.elapsed_ms))
}

#[tauri::command]
fn list_history(state: State<'_, AppState>, limit: Option<u32>) -> Result<Vec<HistoryEntry>, String> {
    state
        .db
        .list(limit.unwrap_or(200))
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn delete_history(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.db.delete(&id).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn clear_history(state: State<'_, AppState>) -> Result<(), String> {
    state.db.clear().map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn get_dictation_state(dictation: State<'_, Arc<Dictation>>) -> StatePayload {
    dictation.snapshot()
}

#[tauri::command]
fn toggle_dictation(dictation: State<'_, Arc<Dictation>>) {
    dictation.toggle();
}

#[tauri::command]
fn cancel_dictation(dictation: State<'_, Arc<Dictation>>) {
    dictation.cancel();
}

#[tauri::command]
fn show_hub(app: AppHandle) {
    show_main_window(&app);
}

// ----- helpers ---------------------------------------------------------------

fn apply_autostart(app: &AppHandle, enabled: bool) -> Result<()> {
    let manager = app.autolaunch();
    if enabled {
        manager.enable().map_err(|e| anyhow!("activation du lancement automatique : {e}"))?;
    } else {
        manager.disable().map_err(|e| anyhow!("désactivation du lancement automatique : {e}"))?;
    }
    log::info!("lancement à l'ouverture de session : {}", if enabled { "activé" } else { "désactivé" });
    Ok(())
}

fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// Charge le modèle whisper courant en arrière-plan pour que la première dictée soit rapide.
fn preload_local_model(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        let (enabled, id, gpu, language, threads) = {
            let s = state.settings.read();
            (s.uses_local_asr(), s.local_model.clone(), s.local_gpu, s.language.clone(), s.local_threads)
        };
        if !enabled {
            return;
        }
        match models::installed_path(&state.models_dir, &id) {
            Some(path) => {
                if let Err(e) = state.local_asr.warm_up(&path, &language, threads, gpu) {
                    log::error!("préchargement du modèle {id} impossible : {e:#}");
                }
            }
            None => log::warn!("modèle whisper « {id} » absent : à télécharger dans Réglages"),
        }
    });
}

/// Enregistre le raccourci push-to-talk et, s'il n'est pas vide, celui de réécriture.
fn register_shortcuts(app: &AppHandle, spec: &str, polish_spec: &str) -> Result<()> {
    let shortcut: Shortcut = spec
        .parse()
        .map_err(|e| anyhow!("raccourci invalide « {spec} » : {e}"))?;
    let polish: Option<Shortcut> = if polish_spec.trim().is_empty() {
        None
    } else {
        Some(
            polish_spec
                .parse()
                .map_err(|e| anyhow!("raccourci de réécriture invalide « {polish_spec} » : {e}"))?,
        )
    };
    if polish == Some(shortcut) {
        bail!("le raccourci de réécriture doit être différent du raccourci de dictée");
    }
    let gs = app.global_shortcut();
    gs.unregister_all()
        .map_err(|e| anyhow!("désenregistrement des raccourcis : {e}"))?;
    gs.register(shortcut)
        .map_err(|e| anyhow!("enregistrement de « {spec} » : {e} (déjà utilisé par une autre application ?)"))?;
    log::info!("raccourci push-to-talk : {spec}");
    if let Some(p) = polish {
        gs.register(p).map_err(|e| {
            anyhow!("enregistrement de « {polish_spec} » : {e} (déjà utilisé par une autre application ?)")
        })?;
        log::info!("raccourci de réécriture : {polish_spec}");
    }
    if let Some(state) = app.try_state::<AppState>() {
        *state.polish_shortcut.lock() = polish;
    }
    Ok(())
}

fn setup_tray(app: &AppHandle) -> Result<()> {
    let open = MenuItem::with_id(app, "open", "Ouvrir Dictae", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let icon = app
        .default_window_icon()
        .cloned()
        .context("icône d'application absente")?;
    TrayIconBuilder::with_id("dictae-tray")
        .icon(icon)
        .tooltip(format!("Dictae {}", app.package_info().version))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// Place la flow bar en bas au centre de l'écran principal.
fn position_flowbar(app: &AppHandle) {
    let Some(win) = app.get_webview_window("flowbar") else {
        return;
    };
    let Ok(Some(monitor)) = win.primary_monitor() else {
        return;
    };
    let Ok(size) = win.outer_size() else {
        return;
    };
    let area = monitor.work_area();
    let x = area.position.x + (area.size.width as i32 - size.width as i32) / 2;
    let y = area.position.y + area.size.height as i32 - size.height as i32 - 12;
    if let Err(e) = win.set_position(PhysicalPosition::new(x, y)) {
        log::warn!("positionnement de la flow bar impossible : {e}");
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::Builder::new().args([HIDDEN_ARG]).build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    let is_polish = app
                        .try_state::<AppState>()
                        .map(|s| s.polish_shortcut.lock().as_ref() == Some(shortcut))
                        .unwrap_or(false);
                    if let Some(dictation) = app.try_state::<Arc<Dictation>>() {
                        match (event.state, is_polish) {
                            (ShortcutState::Pressed, false) => dictation.on_shortcut_pressed(),
                            (ShortcutState::Released, false) => dictation.on_shortcut_released(),
                            (ShortcutState::Pressed, true) => dictation.on_polish_pressed(),
                            (ShortcutState::Released, true) => dictation.on_polish_released(),
                        }
                    }
                })
                .build(),
        )
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .context("dossier de données introuvable")?;
            std::fs::create_dir_all(&data_dir)?;
            log::info!("données : {}", data_dir.display());

            let settings_path = data_dir.join("settings.json");
            let settings = Settings::load(&settings_path);
            let db = Db::open(&data_dir.join("dictae.sqlite"))?;
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .connect_timeout(Duration::from_secs(10))
                .user_agent(concat!("Dictae/", env!("CARGO_PKG_VERSION")))
                .build()?;

            let shortcut_spec = settings.shortcut.clone();
            let polish_spec = settings.polish_shortcut.clone();
            let llama_exe = app
                .path()
                .resolve("resources/llama/llama-server.exe", tauri::path::BaseDirectory::Resource)
                .unwrap_or_else(|_| PathBuf::from("llama-server.exe"));
            log::info!(
                "llama-server : {} ({})",
                llama_exe.display(),
                if llama_exe.is_file() { "présent" } else { "absent" }
            );
            // Un llama-server d'une instance précédente (tuée par l'installeur ou un plantage)
            // garderait la mémoire vidéo et ralentirait tout.
            let orphans = procguard::kill_orphans(&llama_exe);
            if orphans > 0 {
                log::warn!("{orphans} llama-server orphelin(s) terminé(s) au démarrage");
            }
            let llama = llama::LlamaServer::new(llama_exe, http.clone());
            app.manage(AppState {
                settings: RwLock::new(settings),
                settings_path,
                models_dir: data_dir.join("models"),
                db,
                http,
                local_asr: LocalAsr::new(),
                llama,
                downloading: Mutex::new(Vec::new()),
                polish_shortcut: Mutex::new(None),
            });

            autocomplete_onboarding(&app.state::<AppState>());

            let recorder = Recorder::spawn();
            let dictation = Dictation::new(app.handle().clone(), recorder);
            app.manage(dictation);

            if let Err(e) = register_shortcuts(app.handle(), &shortcut_spec, &polish_spec) {
                log::error!("{e:#}");
            }
            if let Err(e) = setup_tray(app.handle()) {
                log::error!("zone de notification : {e:#}");
            }
            position_flowbar(app.handle());
            preload_local_model(app.handle());
            prestart_llama(app.handle());
            if std::env::args().any(|a| a == HIDDEN_ARG) {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.hide();
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Fermer le hub le cache seulement : l'app vit dans la zone de notification.
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            test_asr,
            list_local_models,
            download_local_model,
            delete_local_model,
            list_ollama_models,
            test_format,
            list_history,
            delete_history,
            clear_history,
            list_vocabulary,
            save_vocabulary,
            delete_vocabulary,
            list_input_devices,
            llama_status,
            start_mic_test,
            stop_mic_test,
            set_onboarding_completed,
            get_app_info,
            check_update,
            install_update,
            get_dictation_state,
            toggle_dictation,
            cancel_dictation,
            show_hub,
        ])
        .build(tauri::generate_context!())
        .expect("erreur au lancement de Dictae")
        .run(|app, event| {
            // Arrêter le serveur llama.cpp avec l'application, quel que soit le chemin de sortie.
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app.try_state::<AppState>() {
                    state.llama.stop_blocking();
                }
            }
        });
}
