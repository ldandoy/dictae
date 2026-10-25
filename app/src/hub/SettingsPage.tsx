import { useEffect, useState } from "react";
import {
  api,
  onModelProgress,
  type LlamaStatus,
  type ModelKind,
  type ModelProgress,
  type ModelStatus,
  type Settings,
} from "../lib/api";

const PRESETS: { label: string; baseUrl: string; model: string }[] = [
  { label: "OpenAI", baseUrl: "https://api.openai.com/v1", model: "gpt-4o-mini-transcribe" },
  { label: "Groq (Whisper)", baseUrl: "https://api.groq.com/openai/v1", model: "whisper-large-v3-turbo" },
  { label: "Serveur local", baseUrl: "http://127.0.0.1:8000/v1", model: "Systran/faster-whisper-large-v3" },
];

const fmtBytes = (n: number) => (n >= 1e9 ? `${(n / 1e9).toFixed(2)} Go` : `${Math.round(n / 1e6)} Mo`);

type Toast = { kind: "ok" | "bad" | ""; text: string };

export function SettingsPage({ onSaved }: { onSaved: (s: Settings) => void }) {
  const [s, setS] = useState<Settings | null>(null);
  const [toast, setToast] = useState<Toast>({ kind: "", text: "" });
  const [busy, setBusy] = useState(false);
  const [devices, setDevices] = useState<string[]>([]);

  useEffect(() => {
    api.getSettings().then(setS);
    api.listInputDevices().then(setDevices).catch(() => setDevices([]));
  }, []);

  if (!s) return <div className="empty-state">Chargement…</div>;

  const set = <K extends keyof Settings>(k: K, v: Settings[K]) => setS({ ...s, [k]: v });

  const run = async (label: string, fn: () => Promise<string>) => {
    setBusy(true);
    setToast({ kind: "", text: label });
    try {
      setToast({ kind: "ok", text: await fn() });
    } catch (e) {
      setToast({ kind: "bad", text: String(e) });
    } finally {
      setBusy(false);
    }
  };

  const save = () =>
    run("Enregistrement…", async () => {
      await api.saveSettings(s);
      onSaved(s);
      return "Réglages enregistrés.";
    });

  return (
    <>
      <h1>Réglages</h1>
      <p className="sub">Transcription, mise en forme, raccourci et collage. Tout est stocké localement.</p>

      <div className="form">
        <div className="card" style={{ display: "grid", gap: 12 }}>
          <strong>Microphone et retours</strong>
          <div className="field">
            <label>Microphone</label>
            <div className="row">
              <select value={s.inputDevice} onChange={(e) => set("inputDevice", e.target.value)} style={{ flex: 1 }}>
                <option value="">Micro par défaut de Windows</option>
                {(s.inputDevice && !devices.includes(s.inputDevice) ? [s.inputDevice, ...devices] : devices).map((d) => (
                  <option key={d} value={d}>
                    {d}
                  </option>
                ))}
              </select>
              <button
                className="btn small"
                onClick={() => api.listInputDevices().then(setDevices).catch(() => undefined)}
              >
                Rafraîchir
              </button>
            </div>
          </div>
          <div className="field check">
            <input
              id="sounds"
              type="checkbox"
              checked={s.soundsEnabled}
              onChange={(e) => set("soundsEnabled", e.target.checked)}
            />
            <label htmlFor="sounds">Jouer un son au début et à la fin de la dictée</label>
          </div>
          <div className="field check">
            <input
              id="mute-others"
              type="checkbox"
              checked={s.muteOthers}
              onChange={(e) => set("muteOthers", e.target.checked)}
            />
            <label htmlFor="mute-others">
              Couper le son des autres applications pendant l'enregistrement (musique, vidéos), rétabli ensuite
            </label>
          </div>
          <div className="field check">
            <input
              id="autostart"
              type="checkbox"
              checked={s.launchAtLogin}
              onChange={(e) => set("launchAtLogin", e.target.checked)}
            />
            <label htmlFor="autostart">Lancer Dictae à l'ouverture de session (réduit dans la zone de notification)</label>
          </div>
        </div>

        <div className="card" style={{ display: "grid", gap: 12 }}>
          <strong>Transcription</strong>
          <div className="row">
            <label className="radio">
              <input
                type="radio"
                name="provider"
                checked={s.asrProvider !== "openai"}
                onChange={() => set("asrProvider", "local")}
              />
              Locale, hors ligne (whisper.cpp intégré)
            </label>
            <label className="radio">
              <input
                type="radio"
                name="provider"
                checked={s.asrProvider === "openai"}
                onChange={() => set("asrProvider", "openai")}
              />
              API compatible OpenAI
            </label>
          </div>

          {s.asrProvider !== "openai" ? (
            <LocalModels kind="whisper" selected={s.localModel} onSelect={(id) => set("localModel", id)} />
          ) : (
            <>
              <div className="presets">
                {PRESETS.map((p) => (
                  <button
                    key={p.label}
                    className="btn small"
                    onClick={() => setS({ ...s, asrBaseUrl: p.baseUrl, asrModel: p.model })}
                  >
                    {p.label}
                  </button>
                ))}
              </div>
              <div className="field">
                <label>URL de base</label>
                <input type="text" value={s.asrBaseUrl} onChange={(e) => set("asrBaseUrl", e.target.value)} />
                <div className="hint">Le client appelle <code>{"{URL}"}/audio/transcriptions</code>.</div>
              </div>
              <div className="field">
                <label>Clé API</label>
                <input
                  type="password"
                  value={s.asrApiKey}
                  onChange={(e) => set("asrApiKey", e.target.value)}
                  placeholder="sk-…"
                  autoComplete="off"
                />
              </div>
              <div className="field">
                <label>Modèle</label>
                <input type="text" value={s.asrModel} onChange={(e) => set("asrModel", e.target.value)} />
              </div>
              <div className="row">
                <button
                  className="btn small"
                  disabled={busy}
                  onClick={() =>
                    run("Test de la connexion…", async () => {
                      await api.saveSettings(s);
                      onSaved(s);
                      return await api.testAsr();
                    })
                  }
                >
                  Enregistrer et tester la connexion
                </button>
              </div>
            </>
          )}

          {s.asrProvider !== "openai" && (
            <div className="field check">
              <input id="gpu" type="checkbox" checked={s.localGpu} onChange={(e) => set("localGpu", e.target.checked)} />
              <label htmlFor="gpu">
                Utiliser la carte graphique (Vulkan) pour whisper : large-v3-turbo devient rapide ; décochez en cas
                de problème d'affichage ou de pilote
              </label>
            </div>
          )}
          <div className="field check">
            <input
              id="streaming"
              type="checkbox"
              checked={s.streamingEnabled}
              onChange={(e) => set("streamingEnabled", e.target.checked)}
            />
            <label htmlFor="streaming">
              Transcrire pendant l'enregistrement (segments aux pauses) : le texte arrive presque aussitôt
            </label>
          </div>
          <div className="row">
            <div className="field" style={{ flex: 1 }}>
              <label>Langue</label>
              <select value={s.language} onChange={(e) => set("language", e.target.value)}>
                <option value="">Détection automatique</option>
                <option value="fr">Français</option>
                <option value="en">Anglais</option>
                <option value="es">Espagnol</option>
                <option value="de">Allemand</option>
                <option value="it">Italien</option>
                <option value="pt">Portugais</option>
              </select>
            </div>
            {s.asrProvider !== "openai" && (
              <div className="field" style={{ flex: 1 }}>
                <label>Threads CPU (0 = automatique)</label>
                <input
                  type="number"
                  min={0}
                  max={64}
                  value={s.localThreads}
                  onChange={(e) => set("localThreads", Number(e.target.value))}
                />
              </div>
            )}
          </div>
        </div>

        <div className="card" style={{ display: "grid", gap: 12 }}>
          <strong>Mise en forme du texte</strong>
          <div className="field check">
            <input
              id="fmt"
              type="checkbox"
              checked={s.formatEnabled}
              onChange={(e) => set("formatEnabled", e.target.checked)}
            />
            <label htmlFor="fmt">
              Corriger ponctuation, majuscules et hésitations avec un modèle de langue avant de coller
            </label>
          </div>
          <div className="row">
            <label className="radio">
              <input
                type="radio"
                name="fmt-provider"
                checked={s.formatProvider !== "ollama"}
                onChange={() => set("formatProvider", "llama")}
              />
              Intégré (llama.cpp, dans Dictae, recommandé)
            </label>
            <label className="radio">
              <input
                type="radio"
                name="fmt-provider"
                checked={s.formatProvider === "ollama"}
                onChange={() => set("formatProvider", "ollama")}
              />
              Ollama (serveur externe)
            </label>
          </div>
          {s.formatProvider !== "ollama" ? (
            <>
              <LocalModels kind="llm" selected={s.llmModel} onSelect={(id) => set("llmModel", id)} />
              <LlamaStatusLine />
            </>
          ) : (
            <>
              <div className="field">
                <label>URL d'Ollama</label>
                <input type="text" value={s.ollamaBaseUrl} onChange={(e) => set("ollamaBaseUrl", e.target.value)} />
              </div>
              <OllamaModelPicker
                baseUrl={s.ollamaBaseUrl}
                value={s.ollamaModel}
                onChange={(m) => set("ollamaModel", m)}
              />
            </>
          )}
          <div className="field">
            <label>Consignes supplémentaires (facultatif)</label>
            <textarea
              rows={2}
              value={s.formatInstructions}
              onChange={(e) => set("formatInstructions", e.target.value)}
              placeholder="Ex. : écris les nombres en chiffres, garde le tutoiement…"
            />
          </div>
          <div className="row">
            <button
              className="btn small"
              disabled={busy}
              onClick={() =>
                run("Mise en forme d'un exemple…", async () => {
                  await api.saveSettings(s);
                  onSaved(s);
                  return await api.testFormat();
                })
              }
            >
              Enregistrer et tester sur un exemple
            </button>
          </div>
        </div>

        <div className="card" style={{ display: "grid", gap: 12 }}>
          <strong>Raccourci et collage</strong>
          <div className="field check">
            <input
              id="context"
              type="checkbox"
              checked={s.contextEnabled}
              onChange={(e) => set("contextEnabled", e.target.checked)}
            />
            <label htmlFor="context">
              Lire le contexte de saisie (application, texte autour du curseur) pour la mise en forme, ajuster les
              espaces et vérifier le collage
            </label>
          </div>
          <div className="field">
            <label>Raccourci push-to-talk</label>
            <input type="text" value={s.shortcut} onChange={(e) => set("shortcut", e.target.value)} />
            <div className="hint">
              Format : <code>Ctrl+Shift+Space</code>, <code>Alt+D</code>, <code>Super+Shift+V</code>… Maintenir pour
              dicter ; un appui bref (&lt; {s.handsFreeTapMs} ms) passe en mains libres.
            </div>
          </div>
          <div className="row">
            <div className="field" style={{ flex: 1 }}>
              <label>Seuil mains libres (ms)</label>
              <input
                type="number"
                min={100}
                max={1000}
                value={s.handsFreeTapMs}
                onChange={(e) => set("handsFreeTapMs", Number(e.target.value))}
              />
            </div>
            <div className="field" style={{ flex: 1 }}>
              <label>Délai avant Ctrl+V (ms)</label>
              <input
                type="number"
                min={0}
                max={1000}
                value={s.pasteDelayMs}
                onChange={(e) => set("pasteDelayMs", Number(e.target.value))}
              />
            </div>
          </div>
          <div className="field check">
            <input
              id="restore"
              type="checkbox"
              checked={s.restoreClipboard}
              onChange={(e) => set("restoreClipboard", e.target.checked)}
            />
            <label htmlFor="restore">Restaurer le presse-papiers après le collage</label>
          </div>
        </div>

        <div className="row">
          <button className="btn primary" onClick={save} disabled={busy}>
            Enregistrer
          </button>
        </div>
        {toast.text && <div className={`toast ${toast.kind}`}>{toast.text}</div>}
      </div>
    </>
  );
}

function LlamaStatusLine() {
  const [status, setStatus] = useState<LlamaStatus | null>(null);
  useEffect(() => {
    let alive = true;
    const tick = () => api.llamaStatus().then((s) => alive && setStatus(s)).catch(() => undefined);
    tick();
    const id = setInterval(tick, 3000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);
  if (!status) return null;
  if (!status.available) {
    return <div className="toast bad">Le serveur llama.cpp n'est pas livré avec cette installation ({status.exePath}).</div>;
  }
  return (
    <div className="hint">
      {status.running
        ? `Serveur llama.cpp en marche (${status.model ?? "modèle"}, port ${status.port}).`
        : "Serveur llama.cpp arrêté : il démarre au lancement de Dictae ou à la première dictée."}
    </div>
  );
}

function LocalModels({ kind, selected, onSelect }: { kind: ModelKind; selected: string; onSelect: (id: string) => void }) {
  const [models, setModels] = useState<ModelStatus[]>([]);
  const [progress, setProgress] = useState<Record<string, ModelProgress>>({});
  const [error, setError] = useState<string>("");

  const reload = () =>
    api
      .listLocalModels()
      .then((list) => setModels(list.filter((m) => m.kind === kind)))
      .catch((e) => setError(String(e)));

  useEffect(() => {
    reload();
    let unlisten: (() => void) | undefined;
    onModelProgress((p) => {
      setProgress((prev) => ({ ...prev, [p.id]: p }));
      if (p.done) {
        if (p.error) setError(`Téléchargement de ${p.id} : ${p.error}`);
        reload();
      }
    }).then((u) => (unlisten = u));
    return () => unlisten?.();
  }, []);

  const download = async (id: string) => {
    setError("");
    try {
      await api.downloadLocalModel(id);
      reload();
    } catch (e) {
      setError(String(e));
    }
  };

  const remove = async (id: string) => {
    if (!confirm("Supprimer ce modèle du disque ?")) return;
    await api.deleteLocalModel(id);
    reload();
  };

  return (
    <div className="models">
      {models.map((m) => {
        const p = progress[m.id];
        const pct = p && !p.done && p.total > 0 ? Math.round((100 * p.received) / p.total) : null;
        const isSelected = m.id === selected;
        return (
          <div key={m.id} className={`model ${isSelected ? "selected" : ""}`}>
            <label className="radio">
              <input type="radio" name={`model-${kind}`} checked={isSelected} onChange={() => onSelect(m.id)} />
              <span>
                <strong>{m.label}</strong>
                <span className="muted"> · {fmtBytes(m.bytes)} · {m.note}</span>
              </span>
            </label>
            <span className="model-actions">
              {m.installed ? (
                <>
                  <span className="chip ok">installé</span>
                  <button className="btn small danger" onClick={() => remove(m.id)}>
                    Supprimer
                  </button>
                </>
              ) : m.downloading || pct !== null ? (
                <span className="progress" title={pct !== null ? `${pct} %` : "…"}>
                  <i style={{ width: `${pct ?? 0}%` }} />
                </span>
              ) : (
                <button className="btn small" onClick={() => download(m.id)}>
                  Télécharger
                </button>
              )}
            </span>
          </div>
        );
      })}
      <div className="hint">
        {kind === "whisper"
          ? "Modèles whisper.cpp (GGML) téléchargés depuis Hugging Face dans le dossier de données de Dictae. Le modèle sélectionné est chargé en mémoire au démarrage."
          : "Modèles de langue (GGUF) téléchargés depuis Hugging Face, servis par le llama.cpp intégré. Le modèle sélectionné est chargé sur la carte graphique au démarrage."}
      </div>
      {error && <div className="toast bad">{error}</div>}
    </div>
  );
}

function OllamaModelPicker({
  baseUrl,
  value,
  onChange,
}: {
  baseUrl: string;
  value: string;
  onChange: (m: string) => void;
}) {
  const [models, setModels] = useState<string[]>([]);
  const [status, setStatus] = useState<string>("");

  const refresh = () => {
    setStatus("Interrogation d'Ollama…");
    api
      .listOllamaModels(baseUrl)
      .then((list) => {
        setModels(list);
        setStatus(list.length ? "" : "Aucun modèle installé dans Ollama.");
      })
      .catch((e) => setStatus(String(e)));
  };

  useEffect(() => {
    refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [baseUrl]);

  const options = models.includes(value) || !value ? models : [value, ...models];

  return (
    <div className="field">
      <label>Modèle Ollama</label>
      <div className="row">
        <select value={value} onChange={(e) => onChange(e.target.value)} style={{ flex: 1 }}>
          {!value && <option value="">— choisir —</option>}
          {options.map((m) => (
            <option key={m} value={m}>
              {m}
            </option>
          ))}
        </select>
        <button className="btn small" onClick={refresh}>
          Rafraîchir
        </button>
      </div>
      <div className="hint">
        {status ||
          "Un petit modèle (2 à 4 milliards de paramètres) suffit et répond en moins d'une seconde sur GPU. Installez-en un avec « ollama pull qwen3.5:4b »."}
      </div>
    </div>
  );
}
