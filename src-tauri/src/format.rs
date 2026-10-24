//! Mise en forme du texte dicté (ponctuation, majuscules, hésitations) par un
//! modèle de langue servi par Ollama (`POST /api/chat`).

use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Clone)]
pub struct FormatConfig {
    pub base_url: String,
    pub model: String,
    pub instructions: String,
}

#[derive(Debug)]
pub struct Formatted {
    pub text: String,
    pub elapsed_ms: u64,
}

const SYSTEM_PROMPT: &str = "Tu es le correcteur d'un logiciel de dictée vocale. \
On te donne la transcription brute d'un texte dicté. Rends exactement le même texte, dans la même langue, \
en corrigeant uniquement : la ponctuation, les majuscules, l'écriture des nombres quand des chiffres sont plus naturels, \
et en supprimant les hésitations (euh, hum, bah) et les mots répétés par erreur. \
Ne reformule pas, ne résume pas, n'ajoute rien, ne traduis pas, ne commente pas, ne mets pas de guillemets autour du texte. \
Réponds uniquement avec le texte corrigé.";

fn system_prompt(instructions: &str) -> String {
    let extra = instructions.trim();
    if extra.is_empty() {
        SYSTEM_PROMPT.to_string()
    } else {
        format!("{SYSTEM_PROMPT}\n\nConsignes supplémentaires de l'utilisateur : {extra}")
    }
}

/// Nettoie la réponse du modèle : balises de raisonnement, guillemets englobants, espaces.
fn clean_response(raw: &str) -> String {
    let mut text = raw.to_string();
    while let (Some(start), Some(end)) = (text.find("<think>"), text.find("</think>")) {
        if end > start {
            text.replace_range(start..end + "</think>".len(), "");
        } else {
            break;
        }
    }
    let mut text = text.trim().to_string();
    for (open, close) in [('"', '"'), ('«', '»'), ('“', '”'), ('\'', '\'')] {
        if text.chars().count() >= 2 && text.starts_with(open) && text.ends_with(close) {
            let inner = &text[open.len_utf8()..text.len() - close.len_utf8()];
            // Ne retirer que des guillemets qui englobent tout le texte, pas ceux d'une citation interne.
            if !inner.contains(open) && !inner.contains(close) {
                text = inner.trim().to_string();
            }
        }
    }
    text
}

/// Garde-fou : un modèle qui réécrit, résume ou délire est ignoré au profit du texte brut.
fn looks_plausible(raw: &str, formatted: &str) -> bool {
    let r = raw.chars().count();
    let f = formatted.chars().count();
    if f == 0 {
        return false;
    }
    f <= r * 2 + 40 && f * 3 >= r.saturating_sub(20)
}

pub async fn format_text(client: &reqwest::Client, cfg: &FormatConfig, raw: &str) -> Result<Formatted> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Formatted { text: String::new(), elapsed_ms: 0 });
    }
    if cfg.model.trim().is_empty() {
        bail!("aucun modèle Ollama sélectionné");
    }
    let url = format!("{}/api/chat", cfg.base_url.trim_end_matches('/'));
    let started = Instant::now();
    let body = json!({
        "model": cfg.model,
        "stream": false,
        "think": false,
        "keep_alive": "30m",
        "options": { "temperature": 0, "num_predict": raw.chars().count() * 2 + 64 },
        "messages": [
            { "role": "system", "content": system_prompt(&cfg.instructions) },
            { "role": "user", "content": raw }
        ]
    });
    let response = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("connexion à Ollama ({url})"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Ollama HTTP {} : {}", status.as_u16(), text.chars().take(300).collect::<String>());
    }

    #[derive(Deserialize)]
    struct Message {
        content: String,
    }
    #[derive(Deserialize)]
    struct ChatResponse {
        message: Message,
    }
    let parsed: ChatResponse =
        serde_json::from_str(&text).with_context(|| format!("réponse Ollama inattendue : {}", text.chars().take(200).collect::<String>()))?;

    let cleaned = clean_response(&parsed.message.content);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if !looks_plausible(raw, &cleaned) {
        log::warn!("mise en forme ignorée (réponse implausible) : {cleaned:?}");
        return Ok(Formatted { text: raw.to_string(), elapsed_ms });
    }
    Ok(Formatted { text: cleaned, elapsed_ms })
}

/// Modèles disponibles sur le serveur Ollama (`GET /api/tags`).
pub async fn list_models(client: &reqwest::Client, base_url: &str) -> Result<Vec<String>> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    #[derive(Deserialize)]
    struct Tag {
        name: String,
    }
    #[derive(Deserialize)]
    struct Tags {
        models: Vec<Tag>,
    }
    let tags: Tags = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("connexion à Ollama ({url})"))?
        .error_for_status()
        .context("réponse Ollama en erreur")?
        .json()
        .await
        .context("liste de modèles Ollama illisible")?;
    let mut names: Vec<String> = tags.models.into_iter().map(|t| t.name).collect();
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_response_strips_thinking_and_quotes() {
        assert_eq!(clean_response("<think>blabla</think>\n\"Bonjour, le monde.\""), "Bonjour, le monde.");
        assert_eq!(clean_response("« Salut. »"), "Salut.");
        assert_eq!(clean_response("Il a dit \"oui\" puis \"non\"."), "Il a dit \"oui\" puis \"non\".");
    }

    #[test]
    fn plausibility_guard() {
        assert!(looks_plausible("bonjour le monde", "Bonjour, le monde."));
        assert!(!looks_plausible("bonjour le monde", ""));
        assert!(!looks_plausible("un texte assez long pour que la règle s'applique vraiment ici", "Oui."));
        assert!(!looks_plausible("court", &"x".repeat(200)));
    }

    #[test]
    fn system_prompt_includes_user_instructions() {
        assert!(!system_prompt("  ").contains("Consignes supplémentaires"));
        assert!(system_prompt("tutoie").contains("Consignes supplémentaires de l'utilisateur : tutoie"));
    }
}
