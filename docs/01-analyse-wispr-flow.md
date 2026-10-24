# Analyse de Wispr Flow (Windows 1.6.1253) — base pour Dictae

Analyse faite le 24 octobre 2026 sur l'installation locale
`C:\Users\PC\AppData\Local\WisprFlow\app-1.6.1253` avec `rea` 6.7.0 (CLI), lecture
directe de `app.asar`, des migrations SQL livrées, des chaînes du helper natif et
des fichiers de données dans `%APPDATA%\Wispr Flow`.

Objectif : comprendre comment Wispr Flow est construit pour reconstruire un
équivalent (Dictae), pas pour copier son code ni ses assets.

---

## 1. Vue d'ensemble de la stack

| Couche | Techno observée |
|---|---|
| Shell desktop | Electron 42.11.2 (Chrome 148), packagé avec Electron Forge 7.11 (webpack, Squirrel installer, fuses) |
| UI | React 18 + TypeScript, Zustand (`@zubridge/electron` pour partager le state main/renderer), Lexical (éditeur), Motion, Lottie, i18next |
| Process principal | Node (bundle webpack `.webpack/main/index.js`, ~10 Mo) |
| Base locale | SQLite via `sqlite3` + Sequelize + Umzug (migrations JS livrées dans `resources/migrations`) |
| Encodage audio | Opus via `opusscript` (WASM) dans des Worker threads (`opusEncoder.worker.js`, `incrementalOpusEncoder.worker.js`) |
| Réseau transcription | gRPC (`@grpc/grpc-js` + `@protobuf-ts`) vers `api.wisprflow.ai` / `api-east.wisprflow.ai`, fallback HTTP vers Baseten |
| Auth | Supabase (`@supabase/supabase-js`) + WorkOS (SSO entreprise) |
| Télémétrie | Sentry, PostHog, feature flags distants (~290 flags) |
| Helper natif Windows | `resources/Release/Wispr Flow Helper.exe` : .NET 9/10 **NativeAOT**, CsWin32, 20 Mo, non managé (pas de métadonnées CIL) |
| Extensions | `extensionBootstrap.js`, `agentToolBridge.js`, `@modelcontextprotocol/sdk` : un système d'extensions MCP interne |
| Périphériques | SDK Jabra (`@gnaudio/jabra-js`) pour le push-to-talk casque |

Fuses Electron : tout est à `disabled` (RunAsNode, intégrité asar, etc.). Rien ne bloque l'inspection, mais pas de debug `--inspect` possible.

### Fenêtres (renderers webpack)

| Entrée | Rôle probable |
|---|---|
| `hub` (6 Mo) | Fenêtre principale : historique, dictionnaire, snippets, styles, notes, réunions, paramètres |
| `status` | La « Flow bar » flottante (état micro, niveau, processing) |
| `overlay` | Overlay de dictée / drag |
| `contextMenu` | Menu contextuel |
| `scratchpad` | Bloc-notes vocal |
| `meeting_recorder`, `meeting_ax_inspector` | Notetaker (enregistrement de réunions) |
| `feature_tour`, `accessibility_drop` | Onboarding, guidage permissions |

---

## 2. Pipeline d'une dictée (push-to-talk)

```
 [Helper .NET]                [Electron main]                     [Backend]
 hook clavier bas niveau  --> keybind "ptt" / "popo" (hands-free)
 (SetWindowsHookEx,
  GetAsyncKeyState,
  RawInput)                 --> ouvre session dictée (uuid)
                                 capture micro (getUserMedia +
                                 AudioWorklet dans un renderer)
                                 PCM16 mono 16 kHz, paquets 20 ms
                                 --> encodage Opus (worker WASM)
                                 --> flux gRPC bidirectionnel
                                     TranscriptionService.TranscribeStream
                                       Request{init, context, payload, commit}
                                                                  --> ASR + LLM
                                                                      de formatage
                                     Response{result|state|heartbeat} <--
 UIA: getTextBoxInfo        <-- contexte envoyé au serveur :
 (contents, before_text,        app (name, bundle_id/url, type),
  selected_text, after_text)    textbox, vocabulaire dynamique
                                 (texte écran AX, OCR, noms de
                                 fichiers/variables), conversation
 pasteText                  <-- texte final (html + plaintext)
 (clipboard + SendInput,        sauvegarde History (asr, formatted,
  puis vérification du          pasted, audio opus, latences…)
  texte inséré)
```

Points clés observés :

- **Deux modes de commande** : `ptt` (maintien de la touche), `popo` (hands-free / verrouillé), plus `command` / `instruct` (mode commande vocale) et `lens`.
- **Streaming + fallback** : si pas de heartbeat serveur ou si le retard audio dépasse un seuil, le client lance en parallèle un fallback HTTP (`baseten_http`) par chunks. Le résultat le plus rapide gagne.
- **Le formatage est côté serveur**. Le client n'embarque aucun modèle : pas de Whisper, pas d'ONNX local. Le serveur renvoie `result.text{html, plaintext, num_tokens, languages, confidence}` et des temps (`transcribe`, `format`, `post_process`, `external_asr_time`, `external_llm_time`).
- **Contexte riche** envoyé au serveur : texte de la zone de saisie (avant/sélection/après), contenu écran via accessibilité (`screen_content_ax`), OCR optionnel (`shouldOCRScreenCapture`), capture d'écran, dictionnaire perso + équipe, snippets, style d'écriture par type d'app (`GeneralWritingStyle{other, personal, work, email, slack}`).
- **Insertion du texte** : le helper colle via presse-papiers (sauvegarde/restauration du clipboard) et simule les touches (`SendInput`), puis **vérifie** que le texte est bien dans le champ (`DictatedTextVerification`, `PasteOutcome`). En cas d'échec : notification `PasteFailed` / `PasteBlocked`, et `paste_last_text` permet de recoller.
- **Audio** : constantes `RECORDING_SAMPLE_RATE = 16000`, PCM16, paquets de 20 ms, Opus ~64 kbps, bascule WAV si l'encodeur Opus échoue.

---

## 3. Contrat gRPC `flow_api.v1` (reconstruit depuis le bundle)

Service `TranscriptionService` : `Transcribe` (unaire) et `TranscribeStream` (bidi).

```
Request      { init: Init, context: Context, payload: Payload, commit: enum }
Init         { metadata: Metadata, preferences: Preferences, state }
Metadata     { user_id, session_id, request_id, audio_encoding: enum,
               environment: enum, client: Client{name, platform, version},
               debug, referral_code, always_wait_scribe }
Preferences  { user, language: enum[], vocabulary, replacements: Replacements,
               style_config }
Replacements { replacements_personal: map, replacements_team: map,
               snippets_personal: map, snippets_team: map }
Context      { app: App, textbox: Textbox, vocabulary: DynamicVocabulary,
               user_identifier, content_text, content_html,
               session_dictation_apps: App[], screenshot: bytes,
               conversation: Conversation }
App          { name, bundle_id, url, type: enum }
Textbox      { contents, before_text, selected_text, after_text }
DynamicVocabulary { screen_content_ax[], screen_content_ocr[],
                    variable_names[], file_names[] }
Conversation { id, participants[], messages: ChatMessage{role, content, sender_id}[] }
Payload      { oneof audio: audio_packets: AudioPackets{items: bytes[]}
                           | audio_file: AudioFile{data: bytes} }

Response     { result, state, all_states[], heartbeat: Heartbeat }
Heartbeat    { audio_received_seconds, commit_ack }
Progress     { transcriber, transcriber_finished, formatter, formatter_finished,
               post_processing_finished }
Text         { html, plaintext, num_tokens, languages[], confidence }
CandidateBeam{ text, score, confidence, origin, alignment_score,
               original_text, word_alignments[] }
ResponseTime { transcribe, transcribe_overhead, format, format_overhead,
               post_process, total, external_asr_time, external_llm_time,
               generate_queue_duration, generate_work_duration }
```

Ce schéma est une excellente base pour définir **notre propre** API Dictae : il
sépare bien init / contexte / audio / commit, et il permet de changer de
fournisseur ASR ou LLM sans toucher au client.

---

## 4. Le helper natif Windows (ce qu'Electron ne sait pas faire)

Processus enfant lancé par Electron, **IPC = JSON ligne par ligne sur stdin/stdout**
(avec file d'attente d'écriture côté Electron). Les types sont générés depuis un
JSON Schema partagé (`quicktype` → TS, Swift pour macOS, C# pour Windows).

Responsabilités repérées (chaînes du binaire) :

- **Clavier** : hook bas niveau (`SetWindowsHookEx` + `LowLevelKeyboardProc`), `GetAsyncKeyState`, RawInput, gestion hold / double-tap (`holdThresholdMs`, `doubleTapWindowMs`), messages `registerKeybind`, `setKeyboardCapture`, `keyDown`, `KeypressEvent`.
- **Accessibilité** : UI Automation (`IUIAutomation`, `TextPattern`, `TextRange`), `FocusChangeDetector` (changement d'app / de champ), `getTextBoxInfo`, `subscribeTextBoxChange`, `subscribeCursorPosition`, capture de l'arbre AX en HTML (`AccessibilityTreeHtml`), détection des onglets navigateur (`TabStripMonitor`, `BrowserWindowCandidate`).
- **Insertion** : `pasteText`, `SendInput`, `GetDictatedTextPosition`, `DictatedTextVerification`, `PasteOutcome`, `PasteBlocked`.
- **Audio système** : WASAPI (`IAudioClient`, notifications de session / endpoint) pour savoir **qui tient le micro** (`MicrophoneHoldersChanged`), détecter la lecture média (`IsMediaPlaying`, pour couper la musique pendant la dictée), sons de début/fin de dictée.
- **Processus / fenêtres** : énumération (`Toolhelp32`), `GetWindowRect`, `SetWindowPos`, détection de réunions Teams/Meet/Zoom.
- **Souris** : boutons souris comme déclencheur (« Mouse Flow »).

Sur Windows, c'est ce composant qui fait toute la différence entre un « jouet »
et un vrai outil de dictée système.

---

## 5. Modèle de données local (SQLite `flow.sqlite`)

Tables créées par les migrations livrées :

- `History` : une ligne par dictée. Colonnes marquantes : `asrText`, `formattedText`, `editedText`, `pastedText`, `serverFinalizedText`, `audio` / `opusChunks` (BLOB), `app`, `url`, `platform`, `language`, `detectedLanguage`, `transcriptCommand` (`ptt|popo|lens|command`), `textboxContents`, `axText`, `axHTML`, `duration`, `speechDuration`, `numWords`, `numWordsCorrected`, `e2eLatency`, `clientNetworkLatency`, `editDistanceToDictated`, `usedFallbackAsr`, `fallbackLevel`, `feedback`, `isArchived`, `needsUploading`, `personalizationStyleSettings`, `micDevice`, `appVersion`, `timezoneOffsetMinutes`.
- `Dictionary` : `phrase` (PK), `frequencyUsed/Seen`, `lastUsed/Seen`, `manuallyAdded`, `source`, `observedSource`, `toReplace` + `replacementHtml` (remplacements), `isSnippet`, `isStarred`, `teamDictionaryId`, sync (`id`, `createdAt`, `modifiedAt`, `isDeleted`).
- `Notes`, `NoteVersions`, `NoteImages` (scratchpad), `Meetings`, `MeetingVersions`, `Folders`, `FolderMeetings`, `CalendarEvents` (notetaker), `Polish` (réécriture d'un texte sélectionné), `FlowLensHistory`, `InstructHistory`, `RemoteNotifications`, `Links`.

La base de l'utilisateur fait 578 Mo (audio stocké en BLOB). `rea` refuse de la
lire au-delà de 256 Mo ; les migrations suffisent pour le schéma.

---

## 6. Fonctionnalités (d'après prefs utilisateur, flags et notifications)

Dictée : raccourcis configurables (`shortcuts`, `modifierShortcut`, swap Caps Lock Windows), PTT / hands-free, mode commande (`commandModeEnabled`, `pressEnterCommandEnabled`), formatage IA (`aiFormatting`, `defaultTranscriptionFormat`, `appTranscriptionFormats` par app), langues (`selectedLanguages`, `effectiveLanguages`, détection auto), apprentissage auto du dictionnaire (`shouldAutoLearnWords`), contexte AX (`useAxContext`), OCR écran, styles personnalisés (`personalizationStyles`, `styleDetectionEnabled`), couper/mettre en pause la musique (`shouldMuteAudio`, `shouldPauseAudio`), sons, Flow bar déplaçable / masquable (`statusDockEdge`, `hideFlowBarPermanently`), focus mode (apps/URLs bloquées), intégration IDE (`cursorIdeIntegration`, `ideFileTagging`), Jabra wear detection, micro classés (`rankedAudioDevices`).

Au-delà de la dictée : Polish (réécriture), Scratchpad (notes), Notetaker (réunions, calendrier, résumés), Instruct / Flow Lens (chat contextuel), Insights/stats, équipes et entreprise (dictionnaire équipe, SSO, SCIM), facturation (quota mots hebdo, trial).

---

## 7. Ce que `rea` a apporté et ses limites ici

- `rea` 6.7.0 est installé globalement (npm), CLI fonctionnel. Le serveur MCP et le skill `reverse-engineer-anything` n'étaient **pas** enregistrés : fait avec `rea setup --client claude_code --yes` (entrée `rea` dans `~/.claude.json`, skill dans `~/.claude/skills/`). Actif après redémarrage de Claude Code.
- Utile : `inspect-electron-fuses`, `inspect-managed-artifact` (a montré que le helper est NativeAOT, donc pas décompilable en C#), `doctor`, `analyze-javascript-application` (lancé en arrière-plan sur le `.asar` de 137 Mo).
- Limites : pas de Ghidra/Hopper/IDA installé donc pas de décompilation native ; `inspect-binary-layout` ne supporte que Linux ; `inspect-sqlite-database` limité à 256 Mo. L'essentiel a été obtenu par lecture directe du bundle (constantes, protobuf, canaux IPC) et des chaînes du binaire.

---

## 8. Proposition d'architecture pour Dictae

### Principe

Garder la **séparation** qui fait la force de Wispr Flow :

1. **Client desktop** (UI + orchestration de la dictée).
2. **Helper natif Windows** (hooks clavier, UI Automation, insertion, WASAPI).
3. **Backend de transcription** derrière une API à nous, inspirée de `flow_api.v1`, pour pouvoir changer d'ASR / de LLM librement.

### Option A — Même découpage que Wispr Flow (recommandée si on veut aller vite)

- **Electron + TypeScript + React** : Forge, webpack ou Vite, Zustand, SQLite (`better-sqlite3` + Drizzle ou Kysely plutôt que Sequelize). On sait exactement quelles fenêtres faire : hub, flow bar, overlay, scratchpad.
- **Helper en C# (.NET 9, NativeAOT, CsWin32)** : exactement la bonne boîte à outils pour `SetWindowsHookEx`, `IUIAutomation`, `SendInput`, WASAPI. IPC JSON sur stdin/stdout, schéma partagé (zod côté TS → types C# générés).
- **Backend Node ou Python** : gRPC ou WebSocket bidi, ASR streaming (Deepgram Nova, AssemblyAI, OpenAI Realtime, ou Groq/Whisper par chunks), puis LLM de formatage (Claude Haiku pour la latence) avec le contexte `Textbox` + dictionnaire + style.

### Option B — Un seul langage natif (Tauri 2 + Rust)

- Rust pour le cœur (hotkeys via `rdev`/hooks Win32, `windows-rs` pour UIA et SendInput, `cpal` pour l'audio, `opus` crate), WebView pour l'UI.
- Binaire plus léger (pas 1,5 Go), mais UIA en Rust est plus laborieux et on perd l'écosystème Electron. Plus long à mettre au point.

### Option C — Mode 100 % local

- Même client que A ou B, mais ASR local (whisper.cpp / Parakeet via sherpa-onnx) et LLM local (Ollama est déjà installé sur cette machine). Latence et qualité moindres qu'un backend cloud, mais zéro coût et confidentialité totale. Peut être une **cible du backend** derrière la même API plutôt qu'une architecture à part.

### Découpage en étapes (quelle que soit l'option)

1. **MVP dictée** : raccourci PTT global → capture micro 16 kHz → ASR → texte brut collé via presse-papiers + Ctrl+V → ligne dans `History`. Flow bar minimale.
2. **Formatage contextuel** : helper UIA (`getTextBoxInfo`, app active), prompt LLM avec contexte, dictionnaire + remplacements, choix de langue.
3. **Insertion robuste** : sauvegarde/restauration du clipboard, vérification du texte inséré, « recoller le dernier texte », mode hands-free, sons, mute musique.
4. **Hub** : historique, dictionnaire, snippets, styles par app, paramètres, raccourcis.
5. **Plus tard** : Polish, scratchpad, mode commande, extensions MCP.

## 9. Complément : résultat de `rea analyze-javascript-application`

L'analyse statique complète de l'asar a abouti (228 fichiers, 2 419 modules,
8 153 observations, ~208 Mo de JSON) mais avec 44 limites : le worker a épuisé
son tas sur `main/index.js`, les workers Opus et les gros renderers. Elle
confirme la lecture manuelle et ajoute deux points :

- **SDK OpenAI embarqué** : les endpoints `/realtime/transcription_sessions`,
  `/realtime/sessions`, `/audio/transcriptions`, `/chat/completions`,
  `/responses` sont présents, et un flux `audio.append` PCM16 **24 kHz** mono
  alimente une session Realtime. Ce chemin sert au mode voix « instruct » /
  agent, pas à la dictée classique qui passe par `flow_api.v1`.
- **Catalogue de modèles côté client** : `claude-haiku-4-5`, `claude-sonnet-4-5`,
  `claude-opus-4-6`, `gpt-oss-120b` (Cerebras), `gpt-realtime-2`, `gpt-5.5`,
  sélectionnés par feature flags (`instruct-models`, `polish-provider`,
  `quick-summary-model-config`). Utile pour calibrer nos propres choix
  latence / qualité : Haiku pour le formatage en temps réel, un modèle plus
  gros pour Polish et les résumés.

Les 4 API exposées via `contextBridge` sont `electron`, `zubridge`,
`__zubridge_subscriptionValidator` et `__SENTRY_IPC__` : tout le state
main/renderer passe par Zubridge, les canaux IPC nommés sont presque tous des
hooks de test (`e2e:*`).

Fichiers de référence produits pendant l'analyse (hors projet, dans le scratchpad
de la session) : bundle principal extrait, chaînes du helper, sortie `rea`.
