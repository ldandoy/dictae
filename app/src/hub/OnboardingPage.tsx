import { useEffect, useRef, useState } from "react";
import {
  api,
  onDictationLevel,
  onDictationResult,
  onDictationState,
  onModelProgress,
  type DictationState,
  type HistoryEntry,
  type ModelProgress,
  type ModelStatus,
  type Settings,
} from "../lib/api";

const fmtBytes = (n: number) => (n >= 1e9 ? `${(n / 1e9).toFixed(1)} Go` : `${Math.round(n / 1e6)} Mo`);

type Step = 1 | 2 | 3;

export function OnboardingPage({ settings, onDone }: { settings: Settings; onDone: (s: Settings) => void }) {
  const [step, setStep] = useState<Step>(1);

  const finish = async () => {
    const s = await api.setOnboardingCompleted(true);
    onDone(s);
  };

  return (
    <div className="onboarding">
      <div className="onboarding-head">
        <div className="hub-brand">
          <span className="dot" /> Bienvenue dans Dictae
        </div>
        <ol className="steps">
          <li className={step === 1 ? "active" : step > 1 ? "done" : ""}>Modèles</li>
          <li className={step === 2 ? "active" : step > 2 ? "done" : ""}>Micro</li>
          <li className={step === 3 ? "active" : ""}>Premier essai</li>
        </ol>
      </div>

      {step === 1 && <ModelsStep settings={settings} onNext={() => setStep(2)} />}
      {step === 2 && <MicStep onBack={() => setStep(1)} onNext={() => setStep(3)} />}
      {step === 3 && <TryStep settings={settings} onBack={() => setStep(2)} onFinish={finish} />}

      <div className="onboarding-foot">
        <button className="btn small" onClick={finish}>
          Passer l'accueil
        </button>
      </div>
    </div>
  );
}

function ModelsStep({ settings, onNext }: { settings: Settings; onNext: () => void }) {
  const [models, setModels] = useState<ModelStatus[]>([]);
  const [progress, setProgress] = useState<Record<string, ModelProgress>>({});
  const [error, setError] = useState("");

  const reload = () => api.listLocalModels().then(setModels).catch((e) => setError(String(e)));

  useEffect(() => {
    reload();
    let unlisten: (() => void) | undefined;
    onModelProgress((p) => {
      setProgress((prev) => ({ ...prev, [p.id]: p }));
      if (p.done) {
        if (p.error) setError(p.error);
        reload();
      }
    }).then((u) => (unlisten = u));
    return () => unlisten?.();
  }, []);

  const wanted = [settings.localModel, settings.formatEnabled ? settings.llmModel : ""].filter(Boolean);
  const items = wanted.map((id) => models.find((m) => m.id === id)).filter((m): m is ModelStatus => !!m);
  const allInstalled = items.length === wanted.length && items.every((m) => m.installed);
  const anyBusy = items.some((m) => m.downloading || (progress[m.id] && !progress[m.id].done));
  const totalBytes = items.filter((m) => !m.installed).reduce((a, m) => a + m.bytes, 0);

  const downloadAll = async () => {
    setError("");
    for (const m of items) {
      if (!m.installed && !m.downloading) {
        try {
          await api.downloadLocalModel(m.id);
        } catch (e) {
          setError(String(e));
        }
      }
    }
    reload();
  };

  return (
    <section className="card onboarding-step">
      <h1>1. Les modèles tournent sur votre ordinateur</h1>
      <p className="sub">
        Dictae n'envoie rien sur Internet pendant la dictée : il lui faut deux modèles, téléchargés une seule fois.
      </p>
      <div className="models">
        {items.map((m) => {
          const p = progress[m.id];
          const pct = p && !p.done && p.total > 0 ? Math.round((100 * p.received) / p.total) : null;
          return (
            <div key={m.id} className="model">
              <div style={{ flex: 1 }}>
                <strong>{m.kind === "whisper" ? "Reconnaissance vocale" : "Mise en forme"}</strong>
                <span className="muted">
                  {" "}
                  · {m.label} · {fmtBytes(m.bytes)}
                </span>
                <div className="hint">{m.note}</div>
              </div>
              <span className="model-actions">
                {m.installed ? (
                  <span className="chip ok">prêt</span>
                ) : pct !== null || m.downloading ? (
                  <span className="progress" title={pct !== null ? `${pct} %` : "…"}>
                    <i style={{ width: `${pct ?? 0}%` }} />
                  </span>
                ) : (
                  <span className="chip">à télécharger</span>
                )}
              </span>
            </div>
          );
        })}
      </div>
      {error && <div className="toast bad">{error}</div>}
      <div className="row" style={{ marginTop: 14 }}>
        {!allInstalled && (
          <button className="btn primary" onClick={downloadAll} disabled={anyBusy}>
            {anyBusy ? "Téléchargement…" : `Télécharger (${fmtBytes(totalBytes)})`}
          </button>
        )}
        <button className="btn" onClick={onNext} disabled={!allInstalled}>
          Continuer
        </button>
      </div>
      <div className="hint" style={{ marginTop: 10 }}>
        Vous pourrez changer de modèles plus tard dans Réglages. Le modèle de mise en forme demande environ 3 Go de
        mémoire graphique ; sans carte graphique compatible, choisissez le modèle 2B.
      </div>
    </section>
  );
}

function MicStep({ onBack, onNext }: { onBack: () => void; onNext: () => void }) {
  const [devices, setDevices] = useState<string[]>([]);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [testing, setTesting] = useState(false);
  const [level, setLevel] = useState(0);
  const [peak, setPeak] = useState(0);
  const [error, setError] = useState("");
  const testingRef = useRef(false);

  useEffect(() => {
    api.listInputDevices().then(setDevices).catch(() => setDevices([]));
    api.getSettings().then(setSettings);
    const unlisteners: (() => void)[] = [];
    onDictationLevel((l) => {
      if (!testingRef.current) return;
      setLevel(l);
      setPeak((p) => Math.max(p, l));
    }).then((u) => unlisteners.push(u));
    return () => {
      unlisteners.forEach((u) => u());
      if (testingRef.current) api.stopMicTest();
    };
  }, []);

  const start = async () => {
    setError("");
    setPeak(0);
    try {
      if (settings) await api.saveSettings(settings);
      await api.startMicTest();
      testingRef.current = true;
      setTesting(true);
    } catch (e) {
      setError(String(e));
    }
  };

  const stop = async () => {
    await api.stopMicTest();
    testingRef.current = false;
    setTesting(false);
    setLevel(0);
  };

  const pct = Math.min(100, Math.round(Math.pow(level * 6, 0.7) * 100));
  const heard = peak > 0.02;

  return (
    <section className="card onboarding-step">
      <h1>2. Votre micro</h1>
      <p className="sub">Choisissez le micro, lancez le test et parlez : la barre doit bouger.</p>
      {settings && (
        <div className="field" style={{ maxWidth: 480 }}>
          <label>Microphone</label>
          <select
            value={settings.inputDevice}
            disabled={testing}
            onChange={(e) => setSettings({ ...settings, inputDevice: e.target.value })}
          >
            <option value="">Micro par défaut de Windows</option>
            {devices.map((d) => (
              <option key={d} value={d}>
                {d}
              </option>
            ))}
          </select>
        </div>
      )}
      <div className="meter">
        <i style={{ width: `${pct}%` }} />
      </div>
      <div className="hint">
        {testing ? (heard ? "Son détecté, le micro fonctionne." : "En écoute… parlez normalement.") : "Test arrêté."}
      </div>
      {error && <div className="toast bad">{error}</div>}
      <div className="row" style={{ marginTop: 14 }}>
        <button className="btn" onClick={onBack} disabled={testing}>
          Retour
        </button>
        {testing ? (
          <button className="btn danger" onClick={stop}>
            Arrêter le test
          </button>
        ) : (
          <button className="btn primary" onClick={start}>
            Tester le micro
          </button>
        )}
        <button
          className="btn"
          onClick={async () => {
            if (testing) await stop();
            onNext();
          }}
        >
          Continuer
        </button>
      </div>
      <div className="hint" style={{ marginTop: 10 }}>
        Si rien ne bouge : vérifiez que Windows autorise l'accès au micro (Paramètres › Confidentialité › Microphone) et
        que le bon périphérique est choisi.
      </div>
    </section>
  );
}

function TryStep({ settings, onBack, onFinish }: { settings: Settings; onBack: () => void; onFinish: () => void }) {
  const [state, setState] = useState<DictationState | null>(null);
  const [last, setLast] = useState<HistoryEntry | null>(null);
  const [text, setText] = useState("");

  useEffect(() => {
    const unlisteners: (() => void)[] = [];
    onDictationState(setState).then((u) => unlisteners.push(u));
    onDictationResult(setLast).then((u) => unlisteners.push(u));
    return () => unlisteners.forEach((u) => u());
  }, []);

  const phase = state?.phase ?? "idle";
  const ok = last && last.status === "ok";

  return (
    <section className="card onboarding-step">
      <h1>3. Premier essai</h1>
      <p className="sub">
        Cliquez dans la zone ci-dessous, maintenez <kbd>{settings.shortcut}</kbd>, dites une phrase, relâchez. Le texte
        apparaît là où se trouve votre curseur, comme dans n'importe quelle application.
      </p>
      <textarea
        className="try-area"
        rows={4}
        value={text}
        onChange={(e) => setText(e.target.value)}
        placeholder="Cliquez ici, puis maintenez le raccourci et parlez…"
      />
      <div className="hint">
        {phase === "recording"
          ? "Enregistrement… relâchez quand vous avez fini."
          : phase === "processing"
            ? "Transcription et mise en forme…"
            : ok
              ? `Dictée réussie : « ${(last.formattedText ?? last.asrText).slice(0, 80)} ». Un appui bref sur le raccourci passe en mode mains libres.`
              : last
                ? `Résultat : ${last.status}${last.error ? " · " + last.error : ""}`
                : "La flow bar en bas de l'écran affiche l'état de la dictée et le texte reconnu en direct."}
      </div>
      <div className="row" style={{ marginTop: 14 }}>
        <button className="btn" onClick={onBack}>
          Retour
        </button>
        <button className="btn primary" onClick={onFinish}>
          {ok ? "Terminer" : "Terminer sans essai"}
        </button>
      </div>
    </section>
  );
}
