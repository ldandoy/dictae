# Dictae

Dictée vocale système pour Windows, 100 % locale : maintenez un raccourci, parlez,
relâchez, le texte est transcrit par whisper.cpp, mis en forme par un modèle Ollama et
collé dans l'application active.

```
app/    l'application de bureau (Tauri 2 + Rust + React)
web/    le site de présentation et de téléchargement (à venir)
docs/   analyse de référence de Wispr Flow et architecture de Dictae
```

## L'application (`app/`)

### Prérequis (Windows)

- Node.js 22+ et npm
- Rust stable (`rustup`) avec la cible `x86_64-pc-windows-msvc`
- Visual Studio 2022 Build Tools, workload « Développement Desktop en C++ » (linker MSVC, SDK Windows, CMake)
- LLVM (`winget install LLVM.LLVM`) : `libclang` sert à générer les bindings de whisper.cpp ;
  le chemin est fixé dans `app/src-tauri/.cargo/config.toml` (`LIBCLANG_PATH`)
- WebView2 (présent sur Windows 11)
- Ollama avec un petit modèle pour la mise en forme (`ollama pull qwen3.5:4b`), facultatif

### Développement

```bash
cd app
npm install
npm run tauri dev
```

Le hub s'ouvre (historique, dictionnaire, réglages) et une flow bar flottante apparaît en
bas de l'écran. Dans **Réglages › Transcription**, téléchargez un modèle whisper (tout
tourne ensuite hors ligne), puis maintenez `Ctrl+Shift+Space` pour dicter.

La première compilation construit whisper.cpp via CMake : comptez quelques minutes.

### Build

```bash
cd app
npm run tauri build
```

Produit un installateur NSIS (par utilisateur) et un MSI dans
`app/src-tauri/target/release/bundle/`. Le binaire n'est pas signé.

### Structure

```
app/src/                 interface React (hub + flow bar)
  hub/                   historique, dictionnaire, réglages
  flowbar/               pilule flottante : état, niveau micro, texte en direct
  lib/api.ts             commandes et événements Tauri typés
app/src-tauri/src/
  lib.rs                 démarrage Tauri, commandes, raccourci global, tray, autostart
  dictation.rs           machine d'états Idle → Recording → Processing
  audio.rs               capture micro (cpal), rééchantillonnage 16 kHz, WAV
  stream.rs              transcription en continu : segmentation aux pauses, worker
  local_asr.rs           transcription locale whisper.cpp (whisper-rs)
  models.rs              catalogue + téléchargement des modèles GGML
  asr.rs                 client /audio/transcriptions (OpenAI, Groq, serveur local…)
  format.rs              mise en forme ponctuation/majuscules via Ollama
  context.rs             application active, texte autour du curseur, vérification (UIA)
  vocab.rs               termes, remplacements, snippets
  paste.rs               presse-papiers + Ctrl+V simulé (arboard, enigo)
  sounds.rs              sons de retour
  db.rs                  historique et vocabulaire (SQLite)
  settings.rs            réglages JSON
```

Données locales : `%APPDATA%\net.overconsulting.dictae\` (réglages, base SQLite, modèles).

## Publier une version

1. Monter le numéro de version dans `app/package.json`, `app/src-tauri/Cargo.toml` et
   `app/src-tauri/tauri.conf.json`, commiter.
2. Poser un tag et le pousser : `git tag v0.1.0 && git push origin main --tags`.
3. L'action **Release** (`.github/workflows/release.yml`) construit l'installateur NSIS sur
   un runner Windows, le signe pour l'updater et publie la Release GitHub avec `latest.json`.
   Les installations existantes proposent la mise à jour (À propos › Vérifier les mises à jour).

Secrets à créer une fois dans le dépôt (Settings › Secrets and variables › Actions) :

| Secret | Valeur |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | contenu de `~/.tauri/dictae.key` (clé privée de l'updater, jamais commitée) |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | vide (clé générée sans mot de passe) |

La clé publique correspondante est dans `app/src-tauri/tauri.conf.json` (`plugins.updater.pubkey`) ;
l'adresse du point de mise à jour (`plugins.updater.endpoints`) doit pointer vers le dépôt GitHub réel.

## Le site (`web/`)

Page statique (HTML, CSS, JS sans dépendance) publiée sur GitHub Pages par l'action **Pages**
à chaque modification de `web/` (Settings › Pages › Source : GitHub Actions). Le bouton de
téléchargement interroge l'API GitHub pour pointer vers l'installateur de la dernière Release ;
le dépôt est déduit de l'URL `owner.github.io/repo`, avec un repli dans `web/script.js`.

Aperçu local : `python -m http.server 4173 --directory web` puis http://localhost:4173.
