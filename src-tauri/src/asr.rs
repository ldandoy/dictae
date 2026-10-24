//! Client de transcription : n'importe quelle API compatible OpenAI
//! (`POST {base}/audio/transcriptions`, multipart `file` + `model`).

use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct AsrConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub language: String,
    /// Amorce (vocabulaire personnel) transmise dans le champ `prompt`, ou vide.
    pub prompt: String,
}

#[derive(Debug)]
pub struct Transcript {
    pub text: String,
    pub elapsed_ms: u64,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

pub async fn transcribe(client: &reqwest::Client, cfg: &AsrConfig, wav: Vec<u8>) -> Result<Transcript> {
    if cfg.base_url.trim().is_empty() {
        bail!("URL de l'API de transcription non renseignée");
    }
    let url = format!("{}/audio/transcriptions", cfg.base_url.trim_end_matches('/'));
    let started = Instant::now();

    let file = reqwest::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")?;
    let mut form = reqwest::multipart::Form::new()
        .part("file", file)
        .text("model", cfg.model.clone())
        .text("response_format", "json");
    if !cfg.language.trim().is_empty() {
        form = form.text("language", cfg.language.trim().to_string());
    }
    if !cfg.prompt.trim().is_empty() {
        form = form.text("prompt", cfg.prompt.trim().to_string());
    }

    let mut request = client.post(&url).multipart(form);
    if !cfg.api_key.trim().is_empty() {
        request = request.bearer_auth(cfg.api_key.trim());
    }

    let response = request
        .send()
        .await
        .with_context(|| format!("requête vers {url} impossible"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("ASR HTTP {} : {}", status.as_u16(), truncate(&body, 300));
    }

    #[derive(Deserialize)]
    struct Body {
        text: String,
    }
    let parsed: Body = serde_json::from_str(&body)
        .with_context(|| format!("réponse ASR inattendue : {}", truncate(&body, 200)))?;

    Ok(Transcript {
        text: parsed.text,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Faux serveur HTTP : accepte une requête, renvoie `body` avec `status`,
    /// et retourne la requête brute reçue.
    async fn mock_server(status: &'static str, body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&received);
                if let Some(idx) = text.find("\r\n\r\n") {
                    let headers = &text[..idx];
                    let len = headers
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                        .unwrap_or(0);
                    if received.len() >= idx + 4 + len {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
            String::from_utf8_lossy(&received).into_owned()
        });
        (format!("http://{addr}/v1"), handle)
    }

    fn cfg(base_url: String) -> AsrConfig {
        AsrConfig {
            base_url,
            api_key: "sk-test".into(),
            model: "whisper-test".into(),
            language: "fr".into(),
            prompt: "Dictae, REA".into(),
        }
    }

    #[tokio::test]
    async fn transcribe_posts_multipart_and_reads_text() {
        let (base, server) = mock_server("200 OK", r#"{"text":"bonjour le monde"}"#).await;
        let client = reqwest::Client::new();
        let t = transcribe(&client, &cfg(base), b"RIFF....".to_vec()).await.unwrap();
        assert_eq!(t.text, "bonjour le monde");
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /v1/audio/transcriptions"), "{request}");
        assert!(request.contains("Authorization: Bearer sk-test") || request.contains("authorization: Bearer sk-test"));
        assert!(request.contains("name=\"model\""));
        assert!(request.contains("whisper-test"));
        assert!(request.contains("name=\"language\""));
        assert!(request.contains("name=\"prompt\""));
        assert!(request.contains("Dictae, REA"));
        assert!(request.contains("filename=\"audio.wav\""));
    }

    #[tokio::test]
    async fn transcribe_reports_http_errors() {
        let (base, _server) = mock_server("401 Unauthorized", r#"{"error":"bad key"}"#).await;
        let client = reqwest::Client::new();
        let err = transcribe(&client, &cfg(base), vec![0; 16]).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("401"), "{msg}");
        assert!(msg.contains("bad key"), "{msg}");
    }

    #[tokio::test]
    async fn probe_hits_models_endpoint() {
        let (base, server) = mock_server("200 OK", r#"{"data":[]}"#).await;
        let client = reqwest::Client::new();
        let msg = probe(&client, &cfg(base)).await.unwrap();
        assert!(msg.contains("HTTP 200"), "{msg}");
        assert!(server.await.unwrap().starts_with("GET /v1/models"));
    }
}

/// Vérifie l'URL et la clé en appelant `GET {base}/models`.
pub async fn probe(client: &reqwest::Client, cfg: &AsrConfig) -> Result<String> {
    let url = format!("{}/models", cfg.base_url.trim_end_matches('/'));
    let mut request = client.get(&url);
    if !cfg.api_key.trim().is_empty() {
        request = request.bearer_auth(cfg.api_key.trim());
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("connexion à {url} impossible"))?;
    let status = response.status();
    if status.is_success() {
        Ok(format!("Connexion OK ({url} → HTTP {})", status.as_u16()))
    } else {
        let body = response.text().await.unwrap_or_default();
        bail!("HTTP {} sur {url} : {}", status.as_u16(), truncate(&body, 200))
    }
}
