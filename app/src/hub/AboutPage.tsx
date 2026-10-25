import { useEffect, useState } from "react";
import { api, type AppInfo, type UpdateInfo } from "../lib/api";

export function AboutPage() {
  const [app, setApp] = useState<AppInfo | null>(null);
  const [info, setInfo] = useState<UpdateInfo | null>(null);

  useEffect(() => {
    api.getAppInfo().then(setApp).catch(() => undefined);
  }, []);
  const [status, setStatus] = useState<{ kind: "ok" | "bad" | ""; text: string }>({ kind: "", text: "" });
  const [busy, setBusy] = useState(false);

  const check = async () => {
    setBusy(true);
    setStatus({ kind: "", text: "Vérification…" });
    try {
      const r = await api.checkUpdate();
      setInfo(r);
      setStatus(
        r.available
          ? { kind: "ok", text: `Version ${r.available} disponible (vous avez la ${r.currentVersion}).` }
          : { kind: "ok", text: `Dictae ${r.currentVersion} est à jour.` },
      );
    } catch (e) {
      setStatus({ kind: "bad", text: String(e) });
    } finally {
      setBusy(false);
    }
  };

  const install = async () => {
    setBusy(true);
    setStatus({ kind: "", text: "Téléchargement et installation… Dictae va redémarrer." });
    try {
      await api.installUpdate();
    } catch (e) {
      setStatus({ kind: "bad", text: String(e) });
      setBusy(false);
    }
  };

  return (
    <>
      <h1>À propos</h1>
      <p className="sub">Dictae {app?.version ?? ""} — dictée vocale système pour Windows, 100 % locale.</p>
      {app && (
        <div className="card" style={{ maxWidth: 560, display: "grid", gap: 6, marginBottom: 12, fontSize: 13 }}>
          <div>
            <span className="muted">Version : </span>
            <strong>{app.version}</strong>
            <span className="muted"> · moteur whisper : {app.whisperBackends.join(" + ")}</span>
          </div>
          <div>
            <span className="muted">Modèle chargé : </span>
            {app.loadedModel ?? "aucun"}
          </div>
          <div className="muted" style={{ userSelect: "text" }}>Programme : {app.exePath}</div>
          <div className="muted" style={{ userSelect: "text" }}>Données : {app.dataDir}</div>
          <div className="muted" style={{ userSelect: "text" }}>Journal : {app.logPath}</div>
        </div>
      )}
      <div className="card" style={{ maxWidth: 560, display: "grid", gap: 10 }}>
        <p style={{ margin: 0 }}>
          Dictae écoute le micro pendant que vous maintenez le raccourci, transcrit avec whisper.cpp sur votre
          machine, met en forme avec un modèle Ollama local, puis colle le texte dans l'application active. Rien ne
          quitte votre ordinateur.
        </p>
        <p style={{ margin: 0, color: "var(--text-muted)" }}>
          Cœur natif : Tauri 2 + Rust (whisper-rs, cpal, enigo, arboard, rusqlite). Interface : React. Données dans{" "}
          <code>%APPDATA%\net.overconsulting.dictae</code>.
        </p>
        <div className="row">
          <button className="btn" onClick={check} disabled={busy}>
            Vérifier les mises à jour
          </button>
          {info?.available && (
            <button className="btn primary" onClick={install} disabled={busy}>
              Installer la version {info.available}
            </button>
          )}
        </div>
        {status.text && <div className={`toast ${status.kind}`}>{status.text}</div>}
        {info?.notes && (
          <pre className="notes" style={{ whiteSpace: "pre-wrap", margin: 0, color: "var(--text-muted)" }}>
            {info.notes}
          </pre>
        )}
      </div>
    </>
  );
}
