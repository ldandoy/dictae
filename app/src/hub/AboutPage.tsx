export function AboutPage() {
  return (
    <>
      <h1>À propos</h1>
      <p className="sub">Dictae 0.1.0 — dictée vocale système pour Windows.</p>
      <div className="card" style={{ maxWidth: 560, display: "grid", gap: 8 }}>
        <p style={{ margin: 0 }}>
          Dictae écoute le micro pendant que vous maintenez le raccourci, envoie l'audio (WAV 16 kHz mono) à une
          API de transcription compatible OpenAI, puis colle le texte dans l'application active via le
          presse-papiers et <kbd>Ctrl+V</kbd>.
        </p>
        <p style={{ margin: 0, color: "var(--text-muted)" }}>
          Cœur natif : Tauri 2 + Rust (cpal, enigo, arboard, rusqlite). Interface : React. Historique et réglages
          sont stockés dans <code>%APPDATA%\net.overconsulting.dictae</code>.
        </p>
      </div>
    </>
  );
}
