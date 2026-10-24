//! Réglages utilisateur, persistés en JSON dans le dossier de données de l'app.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const PROVIDER_LOCAL: &str = "local";
pub const PROVIDER_OPENAI: &str = "openai";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// `local` (whisper.cpp intégré) ou `openai` (API compatible OpenAI).
    pub asr_provider: String,
    /// Identifiant du modèle whisper local (voir `models::MODELS`).
    pub local_model: String,
    /// Threads pour whisper.cpp ; 0 = automatique (cœurs physiques).
    pub local_threads: u32,
    /// Transcrire par segments pendant l'enregistrement (texte quasi immédiat au relâchement).
    pub streaming_enabled: bool,

    /// Racine d'une API compatible OpenAI (`{base}/audio/transcriptions`).
    pub asr_base_url: String,
    pub asr_api_key: String,
    pub asr_model: String,
    /// Code ISO 639-1 (`fr`, `en`…) ou chaîne vide pour la détection automatique.
    pub language: String,

    /// Mise en forme du texte (ponctuation, majuscules) par un modèle Ollama.
    pub format_enabled: bool,
    pub ollama_base_url: String,
    pub ollama_model: String,
    /// Consignes supplémentaires ajoutées au prompt de mise en forme.
    pub format_instructions: String,

    /// Raccourci global push-to-talk, format `Ctrl+Shift+Space`.
    pub shortcut: String,
    /// Remettre le contenu précédent du presse-papiers après le collage.
    pub restore_clipboard: bool,
    /// Délai entre l'écriture du presse-papiers et l'envoi de Ctrl+V.
    pub paste_delay_ms: u64,
    /// Un appui plus court que ce seuil bascule en mode mains libres.
    pub hands_free_tap_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            asr_provider: PROVIDER_LOCAL.into(),
            // Sur CPU, small répond en ~2,5 s pour 7 s de parole ; large-v3-turbo en ~11 s.
            local_model: "small-q5_1".into(),
            local_threads: 0,
            streaming_enabled: true,
            asr_base_url: "https://api.openai.com/v1".into(),
            asr_api_key: String::new(),
            asr_model: "gpt-4o-mini-transcribe".into(),
            language: "fr".into(),
            format_enabled: true,
            ollama_base_url: "http://127.0.0.1:11434".into(),
            ollama_model: "qwen3.5:4b".into(),
            format_instructions: String::new(),
            shortcut: "Ctrl+Shift+Space".into(),
            restore_clipboard: true,
            paste_delay_ms: 60,
            hands_free_tap_ms: 300,
        }
    }
}

impl Settings {
    pub fn uses_local_asr(&self) -> bool {
        self.asr_provider != PROVIDER_OPENAI
    }

    pub fn load(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<Settings>(&raw) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("settings.json illisible ({e}), valeurs par défaut utilisées");
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("création du dossier de réglages")?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(path, raw).context("écriture de settings.json")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults_for_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("settings.json");
        let mut s = Settings::default();
        s.shortcut = "Alt+D".into();
        s.language = String::new();
        s.save(&path).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.shortcut, "Alt+D");
        assert_eq!(loaded.language, "");
        assert_eq!(loaded.asr_model, s.asr_model);
        assert!(loaded.uses_local_asr());

        // Champs manquants ou fichier partiel → valeurs par défaut (camelCase côté JSON).
        fs::write(&path, r#"{"asrModel":"custom","asrProvider":"openai"}"#).unwrap();
        let partial = Settings::load(&path);
        assert_eq!(partial.asr_model, "custom");
        assert!(!partial.uses_local_asr());
        assert_eq!(partial.shortcut, Settings::default().shortcut);

        fs::write(&path, "pas du json").unwrap();
        assert_eq!(Settings::load(&path).shortcut, Settings::default().shortcut);
    }
}
