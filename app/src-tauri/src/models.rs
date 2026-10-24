//! Catalogue et téléchargement des modèles whisper.cpp (format GGML, dépôt
//! `ggerganov/whisper.cpp` sur Hugging Face).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;

pub const EVENT_PROGRESS: &str = "models://progress";
const BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

pub struct ModelInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub file: &'static str,
    pub bytes: u64,
    pub note: &'static str,
}

/// Du plus précis au plus rapide. Les tailles servent à valider un téléchargement complet.
pub const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "large-v3-turbo-q5_0",
        label: "Large v3 Turbo (q5_0)",
        file: "ggml-large-v3-turbo-q5_0.bin",
        bytes: 574_041_195,
        note: "Meilleure qualité ; sur CPU ≈ 1,5× la durée de la dictée",
    },
    ModelInfo {
        id: "medium-q5_0",
        label: "Medium (q5_0)",
        file: "ggml-medium-q5_0.bin",
        bytes: 539_212_467,
        note: "Très bonne qualité, lent sur CPU",
    },
    ModelInfo {
        id: "small-q5_1",
        label: "Small (q5_1)",
        file: "ggml-small-q5_1.bin",
        bytes: 190_085_487,
        note: "Recommandé sur CPU : ≈ 0,4× la durée de la dictée, bonne qualité",
    },
    ModelInfo {
        id: "base-q5_1",
        label: "Base (q5_1)",
        file: "ggml-base-q5_1.bin",
        bytes: 59_707_625,
        note: "Très rapide, qualité limitée en français",
    },
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    pub id: String,
    pub label: String,
    pub file: String,
    pub bytes: u64,
    pub note: String,
    pub installed: bool,
    pub downloading: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressPayload {
    pub id: String,
    pub received: u64,
    pub total: u64,
    pub done: bool,
    pub error: Option<String>,
}

pub fn find(id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|m| m.id == id)
}

pub fn file_path(models_dir: &Path, info: &ModelInfo) -> PathBuf {
    models_dir.join(info.file)
}

pub fn is_installed(models_dir: &Path, info: &ModelInfo) -> bool {
    std::fs::metadata(file_path(models_dir, info))
        .map(|m| m.is_file() && m.len() == info.bytes)
        .unwrap_or(false)
}

/// Chemin du modèle `id` s'il est présent et complet.
pub fn installed_path(models_dir: &Path, id: &str) -> Option<PathBuf> {
    let info = find(id)?;
    is_installed(models_dir, info).then(|| file_path(models_dir, info))
}

pub fn statuses(models_dir: &Path, downloading: &[String]) -> Vec<ModelStatus> {
    MODELS
        .iter()
        .map(|m| ModelStatus {
            id: m.id.into(),
            label: m.label.into(),
            file: m.file.into(),
            bytes: m.bytes,
            note: m.note.into(),
            installed: is_installed(models_dir, m),
            downloading: downloading.iter().any(|d| d == m.id),
        })
        .collect()
}

pub fn delete(models_dir: &Path, id: &str) -> Result<()> {
    let info = find(id).context("modèle inconnu")?;
    let path = file_path(models_dir, info);
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("suppression de {}", path.display()))?;
    }
    Ok(())
}

/// Télécharge le modèle dans `models_dir` (fichier `.part` puis renommage) en
/// diffusant la progression à l'interface.
pub async fn download(app: &AppHandle, client: &reqwest::Client, models_dir: &Path, info: &ModelInfo) -> Result<PathBuf> {
    tokio::fs::create_dir_all(models_dir).await.context("création du dossier des modèles")?;
    let final_path = file_path(models_dir, info);
    let part_path = models_dir.join(format!("{}.part", info.file));
    let url = format!("{BASE_URL}/{}", info.file);

    let emit = |received: u64, done: bool, error: Option<String>| {
        let _ = app.emit(
            EVENT_PROGRESS,
            ProgressPayload {
                id: info.id.into(),
                received,
                total: info.bytes,
                done,
                error,
            },
        );
    };

    let result: Result<()> = async {
        let response = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("connexion à {url}"))?;
        if !response.status().is_success() {
            bail!("HTTP {} sur {url}", response.status().as_u16());
        }
        let mut file = tokio::fs::File::create(&part_path)
            .await
            .with_context(|| format!("création de {}", part_path.display()))?;
        let mut stream = response.bytes_stream();
        let mut received: u64 = 0;
        let mut last_emit = std::time::Instant::now();
        emit(0, false, None);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("lecture du flux de téléchargement")?;
            file.write_all(&chunk).await.context("écriture du modèle")?;
            received += chunk.len() as u64;
            if last_emit.elapsed().as_millis() >= 200 {
                emit(received, false, None);
                last_emit = std::time::Instant::now();
            }
        }
        file.flush().await?;
        drop(file);
        if received != info.bytes {
            bail!("téléchargement incomplet : {received} octets sur {}", info.bytes);
        }
        tokio::fs::rename(&part_path, &final_path)
            .await
            .context("renommage du fichier téléchargé")?;
        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            emit(info.bytes, true, None);
            Ok(final_path)
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&part_path).await;
            emit(0, true, Some(format!("{e:#}")));
            Err(e)
        }
    }
}
