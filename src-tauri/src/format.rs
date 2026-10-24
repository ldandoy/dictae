//! Mise en forme du texte dicté (ponctuation, majuscules, hésitations) par un
//! modèle de langue servi par Ollama (`POST /api/chat`).

use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::context::InsertionContext;

#[derive(Debug, Clone)]
pub struct FormatConfig {
    pub base_url: String,
    pub model: String,
    pub instructions: String,
    /// Contexte d'insertion (application, texte autour du curseur), s'il a pu être lu.
    pub context: Option<InsertionContext>,
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

fn tail(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(max);
    chars[start..].iter().collect()
}

fn head(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Décrit l'endroit où le texte sera inséré, pour adapter majuscule initiale,
/// ponctuation finale et registre.
fn context_block(ctx: &InsertionContext) -> String {
    let mut lines = Vec::new();
    let mut place = format!("Le texte sera inséré dans l'application « {} »", ctx.app);
    if !ctx.window_title.trim().is_empty() {
        place.push_str(&format!(" (fenêtre : « {} »)", head(ctx.window_title.trim(), 120)));
    }
    if let Some(url) = &ctx.url {
        place.push_str(&format!(", page {}", head(url, 160)));
    }
    place.push('.');
    lines.push(place);

    let before = ctx.before.trim_end_matches(' ');
    if ctx.caret_known && !before.trim().is_empty() {
        lines.push(format!("Texte déjà présent juste avant le point d'insertion : « {} »", tail(before, 300)));
    }
    if ctx.caret_known && !ctx.after.trim().is_empty() {
        lines.push(format!("Texte juste après : « {} »", head(ctx.after.trim_start(), 100)));
    }
    if !ctx.selected.trim().is_empty() {
        lines.push("Le texte dicté remplace une sélection.".to_string());
    }
    lines.push(
        "Adapte seulement la majuscule initiale et la ponctuation finale à ce contexte (pas de majuscule ni de point \
         final si la phrase précédente est inachevée) et le registre (message instantané court, e-mail, document). \
         Ne recopie jamais ce contexte : réponds uniquement avec le texte dicté corrigé."
            .to_string(),
    );
    lines.join("\n")
}

fn system_prompt_with(instructions: &str, context: Option<&InsertionContext>) -> String {
    let mut prompt = SYSTEM_PROMPT.to_string();
    if let Some(ctx) = context {
        prompt.push_str("\n\nContexte :\n");
        prompt.push_str(&context_block(ctx));
    }
    let extra = instructions.trim();
    if !extra.is_empty() {
        prompt.push_str(&format!("\n\nConsignes supplémentaires de l'utilisateur : {extra}"));
    }
    prompt
}

fn system_prompt(instructions: &str) -> String {
    system_prompt_with(instructions, None)
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
            { "role": "system", "content": system_prompt_with(&cfg.instructions, cfg.context.as_ref()) },
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

    #[test]
    fn system_prompt_describes_insertion_context() {
        let ctx = InsertionContext {
            app: "Slack".into(),
            window_title: "général - Overconsulting".into(),
            before: "Salut, je voulais te dire que".into(),
            after: "".into(),
            caret_known: true,
            readable: true,
            ..Default::default()
        };
        let p = system_prompt_with("", Some(&ctx));
        assert!(p.contains("« Slack »"), "{p}");
        assert!(p.contains("juste avant le point d'insertion : « Salut, je voulais te dire que »"), "{p}");
        assert!(!p.contains("Texte juste après"), "{p}");
        let none = system_prompt_with("", Some(&InsertionContext { app: "Bloc-notes".into(), ..Default::default() }));
        assert!(none.contains("Bloc-notes") && !none.contains("point d'insertion"));
    }
}
