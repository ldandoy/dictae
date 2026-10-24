# Dictae

Dictée vocale système pour Windows, inspirée de Wispr Flow : maintenez un raccourci,
parlez, relâchez, le texte est collé dans l'application active.

Cœur natif en **Tauri 2 + Rust**, interface en **React**. Voir
[docs/01-analyse-wispr-flow.md](docs/01-analyse-wispr-flow.md) pour l'analyse de
référence et [docs/02-architecture-dictae.md](docs/02-architecture-dictae.md) pour
l'architecture retenue.

## Prérequis (Windows)

- Node.js 22+ et npm
- Rust stable (`rustup`) avec la cible `x86_64-pc-windows-msvc`
- Visual Studio 2022 Build Tools, workload « Développement Desktop en C++ » (linker MSVC, SDK Windows, CMake)
- LLVM (`winget install LLVM.LLVM`) : `libclang` sert à générer les bindings de whisper.cpp ;
  le chemin est fixé dans `src-tauri/.cargo/config.toml` (`LIBCLANG_PATH`)
- WebView2 (présent sur Windows 11)

## Développement

```bash
npm install
npm run tauri dev
```

Le hub s'ouvre (historique, réglages) et une flow bar flottante apparaît en bas de l'écran.
Dans **Réglages › Transcription**, téléchargez un modèle whisper (tout tourne ensuite hors
ligne), ou choisissez une API compatible OpenAI. Pour la mise en forme du texte, installez
un petit modèle dans Ollama (`ollama pull qwen3.5:4b`) et sélectionnez-le. Puis maintenez
`Ctrl+Shift+Space` pour dicter.

La première compilation construit whisper.cpp via CMake (fourni par les Build Tools) :
comptez quelques minutes.

## Build

```bash
npm run tauri build
```

Produit un installateur NSIS et un MSI dans `src-tauri/target/release/bundle/`.

## Structure

```
src/                 interface React (hub + flow bar)
  hub/               fenêtre principale : historique, réglages
  flowbar/           pilule flottante : état, niveau micro, clic pour dicter
  lib/api.ts         commandes et événements Tauri typés
src-tauri/src/
  lib.rs             démarrage Tauri, commandes, raccourci global, tray
  dictation.rs       machine d'états Idle → Recording → Processing
  audio.rs           capture micro (cpal) → WAV 16 kHz mono
  local_asr.rs       transcription locale whisper.cpp (whisper-rs)
  models.rs          catalogue + téléchargement des modèles GGML
  format.rs          mise en forme ponctuation/majuscules via Ollama
  asr.rs             client /audio/transcriptions (OpenAI, Groq, serveur local…)
  paste.rs           presse-papiers + Ctrl+V simulé (arboard, enigo)
  db.rs              historique SQLite (rusqlite)
  settings.rs        réglages JSON
```
