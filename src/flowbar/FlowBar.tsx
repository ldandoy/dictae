import { useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  api,
  onDictationLevel,
  onDictationState,
  type DictationState,
} from "../lib/api";

const BAR_COUNT = 9;

export function FlowBar() {
  const [state, setState] = useState<DictationState>({
    phase: "idle",
    mode: "push_to_talk",
    message: null,
    error: false,
    generation: 0,
  });
  const [bars, setBars] = useState<number[]>(() => Array(BAR_COUNT).fill(0));
  const history = useRef<number[]>([]);

  useEffect(() => {
    const unlisteners: (() => void)[] = [];
    api.getDictationState().then(setState).catch(() => undefined);
    onDictationState((s) => {
      setState(s);
      if (s.phase !== "recording") {
        history.current = [];
        setBars(Array(BAR_COUNT).fill(0));
      }
    }).then((u) => unlisteners.push(u));
    onDictationLevel((level) => {
      // Niveau RMS 0..1 -> hauteur de barre, avec une mémoire glissante pour l'animation.
      const h = Math.min(1, Math.pow(level * 6, 0.7));
      const next = [...history.current, h].slice(-BAR_COUNT);
      history.current = next;
      const padded = Array(BAR_COUNT - next.length).fill(0).concat(next);
      setBars(padded);
    }).then((u) => unlisteners.push(u));
    return () => unlisteners.forEach((u) => u());
  }, []);

  const phaseClass =
    state.phase === "recording"
      ? "recording"
      : state.phase === "processing"
        ? "processing"
        : state.error
          ? "error"
          : state.message
            ? "ok"
            : "";

  const label =
    state.message ??
    (state.phase === "recording"
      ? state.mode === "hands_free"
        ? "Mains libres"
        : "Enregistrement…"
      : state.phase === "processing"
        ? "Transcription…"
        : "Dictae");

  const onMain = () => {
    if (state.phase !== "processing") api.toggleDictation();
  };

  const onGrip = (e: React.MouseEvent) => {
    if (e.button === 0) getCurrentWindow().startDragging();
  };

  return (
    <div className="fb">
      <div className={`fb-pill ${phaseClass}`}>
        <div className="fb-grip" onMouseDown={onGrip} title="Déplacer">
          <svg width="8" height="14" viewBox="0 0 8 14" fill="currentColor">
            <circle cx="2" cy="2" r="1.3" /><circle cx="6" cy="2" r="1.3" />
            <circle cx="2" cy="7" r="1.3" /><circle cx="6" cy="7" r="1.3" />
            <circle cx="2" cy="12" r="1.3" /><circle cx="6" cy="12" r="1.3" />
          </svg>
        </div>
        <div className="fb-main" onClick={onMain} onDoubleClick={() => api.showHub()} title="Clic : démarrer / terminer · Double-clic : ouvrir Dictae">
          <span className="fb-dot" />
          {state.phase === "recording" ? (
            <span className="fb-bars">
              {bars.map((h, i) => (
                <i key={i} style={{ height: `${4 + h * 18}px` }} />
              ))}
            </span>
          ) : null}
          <span className={`fb-label ${state.error ? "error" : state.phase === "idle" && !state.message ? "muted" : ""}`}>
            {label}
          </span>
        </div>
        {state.phase === "recording" && (
          <button className="fb-cancel" onClick={() => api.cancelDictation()} title="Annuler">
            ✕
          </button>
        )}
      </div>
    </div>
  );
}
