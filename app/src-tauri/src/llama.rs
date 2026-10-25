//! Serveur llama.cpp intégré (`llama-server`, build Vulkan livré dans les ressources),
//! lancé comme processus enfant à la demande et interrogé par son API compatible OpenAI.
//!
//! Un seul serveur à la fois ; il est relancé si le modèle ou le mode GPU change,
//! et arrêté à la fermeture de Dictae.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::{format, procguard};

const CONTEXT_SIZE: u32 = 4096;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);

struct Running {
    child: Child,
    /// Job object « kill on close » : l'enfant meurt avec Dictae, même tué par l'installeur.
    _job: Option<procguard::JobGuard>,
    port: u16,
    model: PathBuf,
    gpu: bool,
    started_at: Instant,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlamaStatus {
    pub available: bool,
    pub running: bool,
    pub model: Option<String>,
    pub port: Option<u16>,
    pub exe_path: String,
}

pub struct LlamaServer {
    exe: PathBuf,
    http: reqwest::Client,
    running: Mutex<Option<Running>>,
}

impl LlamaServer {
    pub fn new(exe: PathBuf, http: reqwest::Client) -> Self {
        Self {
            exe,
            http,
            running: Mutex::new(None),
        }
    }

    pub fn available(&self) -> bool {
        self.exe.is_file()
    }

    pub async fn status(&self) -> LlamaStatus {
        let g = self.running.lock().await;
        LlamaStatus {
            available: self.available(),
            running: g.is_some(),
            model: g
                .as_ref()
                .and_then(|r| r.model.file_name().map(|f| f.to_string_lossy().into_owned())),
            port: g.as_ref().map(|r| r.port),
            exe_path: self.exe.display().to_string(),
        }
    }

    /// Démarre (ou réutilise) le serveur pour ce modèle ; renvoie le port local.
    pub async fn ensure_running(&self, model: &Path, gpu: bool) -> Result<u16> {
        let mut g = self.running.lock().await;
        if let Some(r) = g.as_mut() {
            let alive = r.child.try_wait().map(|s| s.is_none()).unwrap_or(false);
            if alive && r.model == model && r.gpu == gpu {
                if self.healthy(r.port).await {
                    return Ok(r.port);
                }
                log::warn!("llama-server ne répond plus, redémarrage");
            } else if !alive {
                log::warn!("llama-server s'est arrêté, redémarrage");
            }
            let _ = r.child.kill();
            let _ = r.child.wait();
            *g = None;
        }
        if !self.available() {
            bail!("llama-server introuvable : {}", self.exe.display());
        }
        if !model.is_file() {
            bail!("modèle de mise en forme absent : {}", model.display());
        }

        let port = free_port()?;
        let mut cmd = Command::new(&self.exe);
        cmd.args(["--host", "127.0.0.1", "--port", &port.to_string(), "--no-webui", "--jinja"])
            .args(["-m"])
            .arg(model)
            .args(["-c", &CONTEXT_SIZE.to_string()])
            .args(["-ngl", if gpu { "all" } else { "0" }])
            .args(["--reasoning", "off"])
            .args(["--chat-template-kwargs", r#"{"enable_thinking":false}"#])
            .args(["-np", "1", "--flash-attn", "auto"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(dir) = self.exe.parent() {
            cmd.current_dir(dir);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let started_at = Instant::now();
        let mut child = cmd.spawn().with_context(|| format!("lancement de {}", self.exe.display()))?;
        let job = procguard::guard_child(&child);
        log::info!(
            "llama-server démarré (pid {}, port {port}, {}) : {}",
            child.id(),
            if gpu { "GPU" } else { "CPU" },
            model.display()
        );
        if let Some(stderr) = child.stderr.take() {
            std::thread::Builder::new()
                .name("dictae-llama-log".into())
                .spawn(move || {
                    use std::io::{BufRead, BufReader};
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        if line.contains("error") || line.contains("failed") {
                            log::warn!("llama-server : {line}");
                        } else {
                            log::debug!("llama-server : {line}");
                        }
                    }
                })
                .ok();
        }

        // Attendre que le modèle soit chargé.
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                bail!("llama-server s'est arrêté pendant le chargement ({status})");
            }
            if self.healthy(port).await {
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                bail!("llama-server n'a pas répondu en {} s", STARTUP_TIMEOUT.as_secs());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        log::info!("llama-server prêt en {} ms", started_at.elapsed().as_millis());
        *g = Some(Running {
            child,
            _job: job,
            port,
            model: model.to_path_buf(),
            gpu,
            started_at,
        });
        Ok(port)
    }

    /// Première requête à blanc : sur GPU, la première inférence compile les pipelines
    /// Vulkan (5 à 30 s). À appeler après `ensure_running`, hors du chemin de la dictée.
    pub async fn warm_up(&self, port: u16) -> Result<()> {
        let started = Instant::now();
        // Requête de la taille d'une vraie dictée (prompt système + contexte + phrase) :
        // les pipelines Vulkan se compilent par forme de calcul, une requête minuscule
        // laissait la première vraie mise en forme à 30 s.
        let body = serde_json::json!({
            "model": "dictae",
            "stream": false,
            "temperature": 0,
            "max_tokens": 48,
            "chat_template_kwargs": { "enable_thinking": false },
            "messages": format::warm_up_messages()
        });
        self.http
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .json(&body)
            .timeout(Duration::from_secs(180))
            .send()
            .await
            .context("préchauffage de llama-server")?
            .error_for_status()
            .context("préchauffage de llama-server")?;
        log::info!("préchauffage de llama-server terminé en {} ms", started.elapsed().as_millis());
        Ok(())
    }

    async fn healthy(&self, port: u16) -> bool {
        self.http
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    pub async fn stop(&self) {
        let mut g = self.running.lock().await;
        if let Some(mut r) = g.take() {
            log::info!("arrêt de llama-server (pid {}, {} s de fonctionnement)", r.child.id(), r.started_at.elapsed().as_secs());
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
    }

    /// Arrêt synchrone, pour la sortie de l'application.
    pub fn stop_blocking(&self) {
        if let Ok(mut g) = self.running.try_lock() {
            if let Some(mut r) = g.take() {
                let _ = r.child.kill();
                let _ = r.child.wait();
            }
        }
    }
}

impl Drop for LlamaServer {
    fn drop(&mut self) {
        self.stop_blocking();
    }
}

fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("réservation d'un port local")?;
    Ok(listener.local_addr()?.port())
}
