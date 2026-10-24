import { useEffect, useState } from "react";
import { api, type VocabEntry, type VocabKind } from "../lib/api";

const KINDS: { kind: VocabKind; title: string; intro: string; phraseLabel: string; targetLabel: string | null }[] = [
  {
    kind: "term",
    title: "Termes",
    intro:
      "Noms propres, produits, jargon : whisper les reçoit en amorce et Ollama les écrit exactement ainsi.",
    phraseLabel: "Terme",
    targetLabel: null,
  },
  {
    kind: "replacement",
    title: "Remplacements",
    intro: "Quand la transcription contient la phrase de gauche (mots entiers, casse ignorée), elle est remplacée.",
    phraseLabel: "Entendu",
    targetLabel: "Écrire",
  },
  {
    kind: "snippet",
    title: "Snippets",
    intro: "Dites le déclencheur pour insérer un texte complet (signature, adresse, formule…).",
    phraseLabel: "Déclencheur",
    targetLabel: "Texte inséré",
  },
];

const empty = (kind: VocabKind): VocabEntry => ({
  id: "",
  kind,
  phrase: "",
  target: "",
  note: "",
  uses: 0,
  createdAt: "",
  updatedAt: "",
});

export function VocabularyPage() {
  const [entries, setEntries] = useState<VocabEntry[]>([]);
  const [draft, setDraft] = useState<VocabEntry>(empty("term"));
  const [error, setError] = useState("");

  const reload = () => api.listVocabulary().then(setEntries).catch((e) => setError(String(e)));

  useEffect(() => {
    reload();
  }, []);

  const save = async () => {
    setError("");
    try {
      await api.saveVocabulary(draft);
      setDraft(empty(draft.kind));
      reload();
    } catch (e) {
      setError(String(e));
    }
  };

  const remove = async (id: string) => {
    await api.deleteVocabulary(id);
    if (draft.id === id) setDraft(empty(draft.kind));
    reload();
  };

  const section = KINDS.find((k) => k.kind === draft.kind)!;

  return (
    <>
      <h1>Dictionnaire</h1>
      <p className="sub">Vocabulaire personnel, remplacements automatiques et snippets. Stocké localement.</p>

      <div className="card vocab-form">
        <div className="row">
          {KINDS.map((k) => (
            <label key={k.kind} className="radio">
              <input
                type="radio"
                name="vocab-kind"
                checked={draft.kind === k.kind}
                onChange={() => setDraft({ ...empty(k.kind), id: draft.id ? "" : "" })}
              />
              {k.title}
            </label>
          ))}
        </div>
        <div className="hint">{section.intro}</div>
        <div className="row">
          <div className="field" style={{ flex: 1 }}>
            <label>{section.phraseLabel}</label>
            <input
              type="text"
              value={draft.phrase}
              onChange={(e) => setDraft({ ...draft, phrase: e.target.value })}
              placeholder={draft.kind === "snippet" ? "ma signature" : draft.kind === "replacement" ? "over consulting" : "Dictae"}
            />
          </div>
          {section.targetLabel && draft.kind === "replacement" && (
            <div className="field" style={{ flex: 1 }}>
              <label>{section.targetLabel}</label>
              <input
                type="text"
                value={draft.target}
                onChange={(e) => setDraft({ ...draft, target: e.target.value })}
                placeholder="Overconsulting"
              />
            </div>
          )}
        </div>
        {draft.kind === "snippet" && (
          <div className="field">
            <label>{section.targetLabel}</label>
            <textarea
              rows={4}
              value={draft.target}
              onChange={(e) => setDraft({ ...draft, target: e.target.value })}
              placeholder={"Cordialement,\nLaurent Dandoy\nOverconsulting"}
            />
          </div>
        )}
        <div className="field">
          <label>Note (facultatif)</label>
          <input type="text" value={draft.note} onChange={(e) => setDraft({ ...draft, note: e.target.value })} />
        </div>
        <div className="row">
          <button className="btn primary" onClick={save} disabled={!draft.phrase.trim()}>
            {draft.id ? "Mettre à jour" : "Ajouter"}
          </button>
          {draft.id && (
            <button className="btn" onClick={() => setDraft(empty(draft.kind))}>
              Annuler la modification
            </button>
          )}
        </div>
        {error && <div className="toast bad">{error}</div>}
      </div>

      {KINDS.map((k) => {
        const list = entries.filter((e) => e.kind === k.kind);
        return (
          <section key={k.kind} className="vocab-section">
            <h2>
              {k.title} <span className="muted">({list.length})</span>
            </h2>
            {list.length === 0 ? (
              <div className="hint">Aucune entrée.</div>
            ) : (
              <div className="vocab-list">
                {list.map((e) => (
                  <div key={e.id} className={`vocab-item ${draft.id === e.id ? "selected" : ""}`}>
                    <div className="vocab-main">
                      <strong>{e.phrase}</strong>
                      {e.target && (
                        <span className="vocab-target">
                          {" → "}
                          {e.kind === "snippet" ? e.target.split("\n")[0] + (e.target.includes("\n") ? " …" : "") : e.target}
                        </span>
                      )}
                      {e.note && <span className="muted"> · {e.note}</span>}
                    </div>
                    <span className="vocab-actions">
                      <span className="chip" title="Utilisations">
                        {e.uses}×
                      </span>
                      <button className="btn small" onClick={() => setDraft({ ...e })}>
                        Modifier
                      </button>
                      <button className="btn small danger" onClick={() => remove(e.id)}>
                        Supprimer
                      </button>
                    </span>
                  </div>
                ))}
              </div>
            )}
          </section>
        );
      })}
    </>
  );
}
