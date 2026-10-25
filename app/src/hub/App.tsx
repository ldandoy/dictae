import { useEffect, useState } from "react";
import { api, onDictationPartial, onDictationState, type DictationState, type Settings } from "../lib/api";
import { HistoryPage } from "./HistoryPage";
import { SettingsPage } from "./SettingsPage";
import { AboutPage } from "./AboutPage";
import { VocabularyPage } from "./VocabularyPage";
import { OnboardingPage } from "./OnboardingPage";
import { onSettingsChanged } from "../lib/api";

type Page = "history" | "vocabulary" | "settings" | "about";

export function App() {
  const [page, setPage] = useState<Page>("history");
  const [state, setState] = useState<DictationState | null>(null);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [partial, setPartial] = useState("");

  useEffect(() => {
    const unlisteners: (() => void)[] = [];
    api.getDictationState().then(setState).catch(() => undefined);
    api.getSettings().then(setSettings).catch(() => undefined);
    onDictationState((s) => {
      setState(s);
      if (s.phase === "idle") setPartial("");
    }).then((u) => unlisteners.push(u));
    onDictationPartial(setPartial).then((u) => unlisteners.push(u));
    onSettingsChanged(setSettings).then((u) => unlisteners.push(u));
    return () => unlisteners.forEach((u) => u());
  }, []);

  if (settings && !settings.onboardingCompleted) {
    return <OnboardingPage settings={settings} onDone={setSettings} />;
  }

  return (
    <div className="hub">
      <nav className="hub-nav">
        <div className="hub-brand">
          <span className="dot" /> Dictae
        </div>
        <button className={page === "history" ? "active" : ""} onClick={() => setPage("history")}>
          Historique
        </button>
        <button className={page === "vocabulary" ? "active" : ""} onClick={() => setPage("vocabulary")}>
          Dictionnaire
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
        <StatusBanner state={state} shortcut={settings?.shortcut ?? "…"} partial={partial} />
        {page === "history" && <HistoryPage />}
        {page === "vocabulary" && <VocabularyPage />}
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

function StatusBanner({
  state,
  shortcut,
  partial,
}: {
  state: DictationState | null;
  shortcut: string;
  partial: string;
}) {
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
      {partial && phase !== "idle" ? (
        <span className="msg live">{partial}</span>
      ) : state?.message ? (
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
