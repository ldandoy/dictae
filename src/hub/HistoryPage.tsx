import { useCallback, useEffect, useState } from "react";
import { api, onDictationResult, type HistoryEntry } from "../lib/api";

const fmtDate = new Intl.DateTimeFormat("fr-FR", {
  day: "2-digit",
  month: "short",
  hour: "2-digit",
  minute: "2-digit",
});

function statusChip(e: HistoryEntry) {
  switch (e.status) {
    case "ok":
      return <span className="chip ok">collé</span>;
    case "empty":
      return <span className="chip warn">vide</span>;
    case "paste_failed":
      return <span className="chip warn">collage échoué</span>;
    case "paste_unverified":
      return <span className="chip warn">collage non vérifié</span>;
    case "asr_failed":
      return <span className="chip bad">erreur ASR</span>;
    default:
      return <span className="chip">{e.status}</span>;
  }
}

export function HistoryPage() {
  const [items, setItems] = useState<HistoryEntry[]>([]);
  const [loading, setLoading] = useState(true);

  const reload = useCallback(() => {
    api
      .listHistory()
      .then(setItems)
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    reload();
    let unlisten: (() => void) | undefined;
    onDictationResult((entry) => setItems((prev) => [entry, ...prev])).then((u) => (unlisten = u));
    return () => unlisten?.();
  }, [reload]);

  const copy = (text: string) => navigator.clipboard.writeText(text).catch(() => undefined);

  const remove = async (id: string) => {
    await api.deleteHistory(id);
    setItems((prev) => prev.filter((e) => e.id !== id));
  };

  const clearAll = async () => {
    if (!confirm("Supprimer tout l'historique ?")) return;
    await api.clearHistory();
    setItems([]);
  };

  return (
    <>
      <div className="row" style={{ marginBottom: 16 }}>
        <div>
          <h1>Historique</h1>
          <p className="sub" style={{ margin: 0 }}>
            {items.length} dictée{items.length > 1 ? "s" : ""} enregistrée{items.length > 1 ? "s" : ""} localement.
          </p>
        </div>
        <span style={{ marginLeft: "auto" }} className="row">
          <button className="btn small" onClick={reload}>
            Rafraîchir
          </button>
          <button className="btn small danger" onClick={clearAll} disabled={items.length === 0}>
            Tout effacer
          </button>
        </span>
      </div>

      {loading ? (
        <div className="empty-state">Chargement…</div>
      ) : items.length === 0 ? (
        <div className="empty-state">
          Aucune dictée pour l'instant. Maintenez le raccourci, parlez, relâchez : le texte est collé
          dans l'application active et apparaît ici.
        </div>
      ) : (
        <div className="history-list">
          {items.map((e) => (
            <div key={e.id} className="card history-item">
              <div className="meta">
                <span>{fmtDate.format(new Date(e.createdAt))}</span>
                {statusChip(e)}
                {e.app && (
                  <span className="chip" title={[e.windowTitle, e.url].filter(Boolean).join("\n")}>
                    {e.app}
                  </span>
                )}
                <span>{(e.durationMs / 1000).toFixed(1)} s audio</span>
                <span>{e.latencyMs} ms</span>
                <span>{e.model}</span>
                <span className="actions">
                  <button
                    className="btn small"
                    onClick={() => copy(e.formattedText ?? e.asrText)}
                    disabled={!e.asrText}
                  >
                    Copier
                  </button>
                  <button className="btn small danger" onClick={() => remove(e.id)}>
                    Supprimer
                  </button>
                </span>
              </div>
              <div className={`text ${e.asrText ? "" : "empty"}`}>
                {e.formattedText ?? e.asrText ?? "(aucun texte)"}
              </div>
              {e.formattedText && e.formattedText !== e.asrText && (
                <div className="raw" title="Transcription brute avant mise en forme">
                  brut : {e.asrText}
                </div>
              )}
              {e.error && <div className="err">{e.error}</div>}
            </div>
          ))}
        </div>
      )}
    </>
  );
}
