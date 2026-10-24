import { useEffect, useState } from "react";
import { api, onDictationState, type DictationState, type Settings } from "../lib/api";
import { HistoryPage } from "./HistoryPage";
import { SettingsPage } from "./SettingsPage";
import { AboutPage } from "./AboutPage";

type Page = "history" | "settings" | "about";

export function App() {
  const [page, setPage] = useState<Page>("history");
  const [state, setState] = useState<DictationState | null>(null);
  const [settings, setSettings] = useState<Settings | null>(null);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    api.getDictationState().then(setState).catch(() => undefined);
    api.getSettings().then(setSettings).catch(() => undefined);
    onDictationState(setState).then((u) => (unlisten = u));
    return () => unlisten?.();
  }, []);

  return (
    <div className="hub">
      <nav className="hub-nav">
        <div className="hub-brand">
          <span className="dot" /> Dictae
        </div>
        <button className={page === "history" ? "active" : ""} onClick={() => setPage("history")}>
          Historique
        </button>
        <button className={page === "settings" ? "active" : ""} onClick={() => setPage("settings")}>
          Réglages
        </button>
        <div className="spacer" />
        <button className={page === "about" ? "active" : ""} onClick={() => setPage("about")}>
          À propos
        </button>
      </nav>
      <main className="hub-main">
        <StatusBanner state={state} shortcut={settings?.shortcut ?? "…"} />
        {page === "history" && <HistoryPage />}
        {page === "settings" && (
          <SettingsPage
            onSaved={(s) => {
              setSettings(s);
            }}
          />
        )}
        {page === "about" && <AboutPage />}
      </main>
    </div>
  );
}

function StatusBanner({ state, shortcut }: { state: DictationState | null; shortcut: string }) {
  const phase = state?.phase ?? "idle";
  const label =
    phase === "recording"
      ? state?.mode === "hands_free"
        ? "Mains libres"
        : "Enregistrement"
      : phase === "processing"
        ? "Transcription"
        : "Prêt";

  return (
    <div className={`status ${phase}`}>
      <span className="pill">{label}</span>
      {state?.message ? (
        <span className={`msg ${state.error ? "error" : ""}`}>{state.message}</span>
      ) : (
        <span className="msg">
          Maintenez <kbd>{shortcut}</kbd> pour dicter, ou tapez brièvement pour le mode mains libres.
        </span>
      )}
      <span style={{ marginLeft: "auto" }} className="row">
        {phase === "idle" && (
          <button className="btn small" onClick={() => api.toggleDictation()}>
            Démarrer
          </button>
        )}
        {phase === "recording" && (
          <>
            <button className="btn small primary" onClick={() => api.toggleDictation()}>
              Terminer
            </button>
            <button className="btn small danger" onClick={() => api.cancelDictation()}>
              Annuler
            </button>
          </>
        )}
      </span>
    </div>
  );
}
