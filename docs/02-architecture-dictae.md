# Architecture de Dictae (option B : Tauri 2 + Rust)

Décision du 24 octobre 2026 : cœur natif en Rust via Tauri 2, interface WebView en
React. Un seul binaire, pas de helper séparé : les responsabilités que Wispr Flow
délègue à son helper .NET (hooks clavier, collage, plus tard UI Automation) vivent
dans le process Rust.

## Vue d'ensemble

```
 ┌──────────────── WebView (React) ────────────────┐
 │  hub (index.html)        flowbar (flowbar.html) │
 │  historique, réglages    pilule flottante        │
 └───────────▲──────────────────────▲──────────────┘
             │ invoke / events (Tauri IPC)
 ┌───────────┴──────────────────────┴──────────────┐
 │ lib.rs      commandes, tray, raccourci global    │
 │ dictation   Idle → Recording → Processing        │
 │ audio       cpal → PCM mono → 16 kHz → WAV       │
 │ asr         POST {base}/audio/transcriptions     │
 │ paste       arboard (clipboard) + enigo (Ctrl+V) │
 │ db          rusqlite : table history             │
 │ settings    settings.json                         │
 └──────────────────────────────────────────────────┘
```

## Cycle d'une dictée (MVP)

1. Le plugin `global-shortcut` reçoit `Pressed` sur le raccourci (défaut `Ctrl+Shift+Space`).
2. `Dictation::begin` demande au thread audio d'ouvrir le micro par défaut (cpal, WASAPI).
   Le flux est converti en mono f32 à la volée ; un niveau RMS lissé alimente la flow bar
   toutes les 50 ms (`dictation://level`).
3. `Released` : si l'appui a duré moins de `handsFreeTapMs`, la dictée passe en mode
   mains libres (le prochain appui ou un clic termine) ; sinon `finish`.
4. `finish` arrête le flux, rééchantillonne en 16 kHz, encode un WAV PCM16, puis lance
   une tâche async : transcription HTTP, collage, écriture en base.
5. `paste::paste_text` sauvegarde le presse-papiers, y écrit le texte, envoie Ctrl+V par
   `SendInput` (code virtuel `VK_V`, indépendant de la disposition clavier), puis
   restaure l'ancien contenu après 350 ms.
6. Une ligne `history` est écrite (texte, statut, durée audio, latence, modèle, erreur) et
   diffusée au hub (`dictation://result`).

Un compteur `generation` protège contre les résultats périmés : une annulation pendant
la transcription empêche le collage.

## Mode 100 % local (décision du 24 octobre 2026)

Ollama ne fait pas de reconnaissance vocale (il sert des modèles de texte et, depuis
la 0.40.8, de la synthèse vocale). Dictae embarque donc son propre moteur :

- **Transcription : whisper.cpp intégré** via la crate `whisper-rs` (`local_asr.rs`).
  Le modèle GGML est chargé une fois en mémoire au démarrage (`preload_local_model`) et
  réutilisé ; chaque dictée crée un `WhisperState`, reçoit le PCM f32 16 kHz capturé
  (complété à 1,1 s minimum), une amorce ponctuée dans la langue choisie, et renvoie la
  concaténation des segments. Les marqueurs non parlés (`[Musique]`, `(rires)`) sont retirés.
  Sans SDK CUDA ni Vulkan sur la machine, l'inférence tourne sur CPU (cœurs physiques).
- **Catalogue de modèles** (`models.rs`) : large-v3-turbo q5_0 (574 Mo), medium q5_0,
  small q5_1, base q5_1, téléchargés depuis `ggerganov/whisper.cpp` sur Hugging Face
  dans `%APPDATA%\net.overconsulting.dictae\models\`, avec progression diffusée au hub
  (`models://progress`) et validation de la taille.
- **Mise en forme : Ollama** (`format.rs`) : `POST /api/chat` sans raisonnement
  (`think: false`), température 0, prompt système strict (ponctuation, majuscules,
  hésitations ; ni reformulation ni traduction). La réponse est nettoyée (balises
  `<think>`, guillemets englobants) et un garde-fou de longueur rejette une réponse
  implausible au profit du texte brut. Toute erreur Ollama est non bloquante : le brut
  est collé et l'historique note « mise en forme indisponible ».
- Le fournisseur API compatible OpenAI reste disponible dans les réglages.

Mesures du 24 octobre 2026 sur cette machine (Ryzen 9 3900XT 12 cœurs, RTX 3080), fixture
TTS française de 7 s :

| Étape | Modèle | Temps |
|---|---|---|
| whisper.cpp CPU | small q5_1 | 2,5 s |
| whisper.cpp CPU | large-v3-turbo q5_0 | 10,7 s |
| Ollama (GPU) | qwen3.5:4b, modèle chaud | 0,3 s |
| Ollama (GPU) | qwen3.5:4b, premier appel (chargement) | 9 s |
| Ollama | Qwen3.8-27B IQ3_XXS (déborde la VRAM) | 113 s |

Avec le backend **Vulkan** (25 octobre 2026, feature `vulkan` de `whisper-rs`, SDK LunarG
requis à la compilation), même fixture sur la RTX 3080 :

| Modèle | GPU, chaud | GPU, 1er appel après installation du pilote |
|---|---|---|
| small q5_1 | 0,15 s | ≈ 20 s (compilation des pipelines, mise en cache par le pilote) |
| large-v3-turbo q5_0 | 0,25 s | ≈ 12 s |

D'où le défaut `large-v3-turbo-q5_0` et un **préchauffage** au démarrage (inférence sur
1,1 s de silence) qui absorbe la compilation des pipelines. Sans GPU Vulkan, whisper.cpp
retombe sur le CPU ; le réglage « Utiliser la carte graphique » force le CPU si besoin.
Contrainte de build : chemins trop longs pour MSBuild (limite 260 caractères) avec le
sous-projet `vulkan-shaders-gen`, d'où `target-dir = "C:/dt"` dans `.cargo/config.toml`.
Mise en forme : `qwen3.5:4b` (`keep_alive` 30 min pour rester chaud).

## Transcription en continu (24 octobre 2026)

Pour ne pas attendre toute la transcription après le relâchement de la touche,
`stream.rs` transcrit pendant l'enregistrement :

1. Un thread lit le tampon micro brut toutes les 100 ms, le rééchantillonne en 16 kHz
   (`Resampler` incrémental à bornes entières, donc identique quel que soit le découpage).
2. Le `Segmenter` détecte la parole par énergie (seuil = 3,5 × plancher de bruit adaptatif,
   trames de 20 ms) et coupe un segment après 500 ms de silence si au moins 0,8 s de parole
   a été vue ; un segment de plus de 12 s est coupé au point le plus calme des 2 dernières
   secondes. 0,5 s de silence est gardé avant la parole, 0,2 s après.
3. Un worker transcrit les segments **séquentiellement** (un seul whisper à la fois sur
   CPU), avec les 150 derniers caractères déjà transcrits comme amorce pour garder la
   cohérence (casse, ponctuation, noms). Chaque résultat est diffusé en `dictation://partial`
   et affiché dans la flow bar et le hub.
4. Au relâchement, `finish` arrête le micro **sans vider le tampon** (le flux lit encore la
   fin), le flux transcrit le reste et renvoie le texte complet ; en cas d'échec, la
   transcription complète classique prend le relais.

Mesure sur la fixture de 7 s (small, CPU) : texte complet disponible 1,8 s après le
relâchement, contre 2,5 s en bloc ; sur une dictée longue le gain est proportionnel à la
durée, l'attente ne dépendant plus que du dernier segment. Les phrases hallucinées
classiques de whisper sur le silence (« Sous-titres réalisés par… ») sont filtrées.

## Contexte de saisie par UI Automation (24 octobre 2026)

`context.rs` lit, au début de chaque dictée et dans un thread à part (COM MTA) :

- la **fenêtre active** (`GetForegroundWindow`), son titre et l'exécutable du processus,
  traduit en nom lisible (« Google Chrome », « Visual Studio Code »…) ;
- le **champ focalisé** (`IUIAutomation::GetFocusedElement`) : via `TextPattern` la
  sélection courante ou, à défaut, le caret (`TextPattern2::GetCaretRange`), puis 400
  caractères avant et 120 après ; à défaut le texte du `ValuePattern` ; les champs mot de
  passe sont ignorés ;
- pour les navigateurs, l'**URL** : premier contrôle `Edit` de la fenêtre dont la valeur
  ressemble à une URL (barre d'adresse).

Usage :

1. `smart_spacing` ajoute l'espace manquant avant/après et met la première lettre en
   minuscule quand le curseur est au milieu d'une phrase (sauf sigle).
2. Le prompt Ollama reçoit un bloc « Contexte » (application, titre, URL, texte avant/après)
   pour adapter majuscule initiale, ponctuation finale et registre, avec l'interdiction
   explicite de recopier ce contexte.
3. Après le collage, `verify_pasted` relit le champ et cherche le texte collé juste avant le
   caret : statut `paste_unverified` et message « Collé ? » s'il n'y est pas, « Collé ✓
   (vérifié) » sinon ; applications sans UIA (terminaux…) → « Collé ✓ » sans vérification.

L'historique garde application, titre de fenêtre et URL. Le tout se désactive dans les
réglages (« Lire le contexte de saisie »).

## Dictionnaire et snippets (24 octobre 2026)

Table `vocabulary` (`vocab.rs`, page « Dictionnaire » du hub), trois types d'entrées :

| Type | Effet |
|---|---|
| `term` | Nom propre ou jargon : injecté dans l'amorce whisper (160 caractères max, les plus utilisés d'abord) et listé dans le prompt Ollama (« à écrire exactement ainsi ») |
| `replacement` | Phrase entendue → forme écrite, appliquée sur le brut avant la mise en forme (mots entiers, casse ignorée) ; la forme cible rejoint aussi les amorces |
| `snippet` | Déclencheur dicté → texte inséré (multi-ligne), appliqué après la mise en forme pour résister aux retouches du modèle ; une ponctuation qui suivait le déclencheur est absorbée si le texte inséré finit par une ponctuation ou un saut de ligne |

Les remplacements et snippets s'appliquent en une seule passe, la phrase la plus longue
l'emportant à chaque position, sans re-analyser le texte déjà remplacé. Chaque usage
incrémente un compteur qui ordonne les amorces. Le fournisseur API reçoit les termes dans
le champ `prompt` de `/audio/transcriptions`.

## Usage quotidien (24 octobre 2026)

- **Micro** : liste des périphériques d'entrée (cpal) dans les réglages ; vide = micro par
  défaut de Windows, repli sur le défaut si le micro choisi disparaît.
- **Sons** : quatre signaux courts synthétisés à la volée (`sounds.rs`, WAV 16 kHz en
  mémoire) joués par `PlaySound` en asynchrone : début, fin, collé, erreur. Désactivables.
- **Silence des autres applications** (`ducking.rs`) : au début de l'enregistrement, les
  sessions audio Core Audio du périphérique de sortie par défaut (musique, vidéos, jeux…)
  sont coupées, sauf les sons système, notre propre processus et les sessions déjà
  muettes ; elles sont rétablies dès l'arrêt du micro. Attention : `IsSystemSoundsSession`
  renvoie S_OK ou S_FALSE, il faut comparer à S_OK et non tester `is_ok()`.
- **Lancement à l'ouverture de session** : plugin `autostart` avec l'argument `--hidden`,
  qui démarre l'app réduite dans la zone de notification (hub caché, flow bar visible).
- **Installateur** : `npm run tauri build` produit un installateur NSIS (par utilisateur,
  sans élévation) et un MSI dans `src-tauri/target/release/bundle/`. Le binaire n'est pas
  signé : SmartScreen avertira au premier lancement.

## Mise en forme intégrée : llama.cpp (25 octobre 2026, 0.2.0)

Pour ne plus dépendre d'un Ollama installé à côté, Dictae embarque **llama-server**
(build Vulkan officiel de llama.cpp, récupéré par `app/scripts/fetch-llama.mjs` dans
`src-tauri/resources/llama/`, ≈ 66 Mo, livré comme ressource de l'installateur).

- `llama.rs` lance le serveur comme processus enfant à la demande (`--host 127.0.0.1`,
  port libre, `--no-webui`, `-ngl all` ou `0` selon le réglage GPU, `-c 4096`,
  `--reasoning off` et `enable_thinking=false` pour Qwen3), attend `/health`, le relance si
  le modèle ou le mode change, et l'arrête à la sortie (`RunEvent::Exit`).
- Pourquoi un processus et non la crate `llama-cpp-2` en interne : whisper-rs embarque déjà
  son propre ggml statique ; deux ggml de versions différentes dans un binaire = conflits de
  symboles à l'édition de liens. Le processus isole aussi un plantage du pilote GPU.
- `format.rs` parle les deux API : `/api/chat` d'Ollama (toujours disponible en option) et
  `/v1/chat/completions` de llama-server (format OpenAI).
- Catalogue (`models.rs`, kind `llm`) : Qwen3.5 4B Q4_K_M (2,7 Go, recommandé) et 2B.
- Mesures RTX 3080, Qwen3.5 4B : première requête ≈ 22 s (pipelines Vulkan), puis ≈ 0,25 s ;
  d'où le préchauffage au démarrage (`prestart_llama` + `warm_up`). Serveur prêt en 7 s.

## Accueil du premier lancement (0.2.1)

`OnboardingPage` remplace le hub tant que `onboardingCompleted` est faux : (1) téléchargement
en un clic des modèles sélectionnés dans les réglages (whisper + LLM) avec progression,
(2) choix du micro et test de niveau en direct (`start_mic_test` ouvre le micro sans
transcription), (3) premier essai du raccourci dans une zone de texte du hub, confirmé par
`dictation://result`. Au démarrage, si les modèles sont déjà présents, l'accueil est marqué
terminé (utilisateurs existants) ; « Revoir l'accueil » dans À propos le rouvre.

## Réécriture de la sélection (0.3.0)

Un second raccourci global (`polishShortcut`, défaut `Ctrl+Alt+Space`, vide = désactivé)
réutilise toute la mécanique de la dictée (maintenir / appui bref mains libres, flux
whisper, sons, coupure du son des autres applications) mais ce qui est dicté est une
**consigne**, pas un texte :

1. À l'appui, `Dictation::begin(mode, polish = true)` lance l'enregistrement et la lecture
   du contexte UI Automation, forcée même si le contexte est désactivé : `ctx.selected`
   est lu en entier (jusqu'à 20 000 caractères, `SELECTED_MAX`) au lieu des 400 d'avant.
2. Au relâchement, `process_polish` transcrit la consigne (vide si l'appui a duré moins de
   300 ms : la consigne par défaut `polishDefaultInstruction` s'applique alors). Si UIA n'a
   pas fourni la sélection (application sans `TextPattern`), repli par `paste::copy_selection`
   (Ctrl+C simulé avec marqueur dans le presse-papiers, ancien contenu remis), mais seulement
   après `wait_modifiers_released` : les touches du raccourci encore enfoncées transformeraient
   le Ctrl+C en Ctrl+Alt+C. Aucune sélection → statut `no_selection`.
3. `format::polish_text` envoie au même point que la mise en forme (llama.cpp intégré ou
   Ollama) un prompt dédié (`POLISH_PROMPT`) avec le texte sélectionné entre `"""`, la
   consigne, l'application d'origine et le vocabulaire ; `max_tokens` = 3 × la longueur de
   la sélection + 256. Les espaces et sauts de ligne qui bordaient la sélection sont
   conservés autour du résultat ; un résultat identique donne le statut `unchanged`.
4. Le collage (`paste_and_verify`, partagé avec la dictée) remplace la sélection encore
   active dans l'application, puis vérifie par UIA. Message « Remplacé ✓ ».

Historique : colonnes `kind` (`dictation` / `polish`) et `source_text` (texte avant
réécriture) ajoutées par migration ; le hub affiche la consigne et le texte d'origine.
Le raccourci de réécriture est distingué du push-to-talk dans le gestionnaire du plugin
global-shortcut en comparant le `Shortcut` reçu à celui mémorisé dans `AppState`.

## Choix et compromis

- **ASR via API compatible OpenAI** en option : zéro infrastructure, et les presets
  (OpenAI, Groq, serveur local type faster-whisper) se changent dans les réglages.
- **Fichier WAV complet par dictée** plutôt que du streaming : plus simple, latence
  acceptable pour des dictées courtes. Le streaming (WebSocket/gRPC) est l'étape suivante.
- **Rééchantillonnage maison** (moyenne par fenêtre) : suffisant pour la parole ; à
  remplacer par un filtre correct (`rubato`) si la qualité ASR en souffre.
- **Collage par presse-papiers** comme Wispr Flow. Sans vérification UI Automation pour
  l'instant : le statut `paste_failed` et le texte laissé dans le presse-papiers couvrent
  les cas d'échec.
- **Flow bar non focusable** (`focusable: false`) pour que le focus reste dans
  l'application cible. Un grip dédié sert au déplacement ; le reste de la pilule est
  cliquable (démarrer/terminer) et un double-clic ouvre le hub.
- **Fermer le hub le cache** : l'application vit dans la zone de notification.

## Feuille de route après le MVP

1. Vérification du collage et contexte de saisie par UI Automation (`windows` crate :
   `IUIAutomation`, `TextPattern`) ; détection de l'application active (nom, URL navigateur).
2. Formatage contextuel : enrichir le prompt Ollama avec le texte avant/après le curseur,
   le dictionnaire, le style par application ; remplacements et snippets.
   Accélération GPU de whisper.cpp (feature `vulkan` ou `cuda` de `whisper-rs`, SDK requis).
3. Streaming audio et transcription partielle pour réduire la latence perçue.
4. Dictionnaire, snippets, styles, statistiques dans le hub ; export/import.
5. Sons de début/fin, mute de la musique pendant la dictée, choix du micro, mode commande.
6. Signature du binaire et mise à jour automatique (plugin `updater`).

## Données locales

`%APPDATA%\net.overconsulting.dictae\` : `settings.json` (clé API comprise, en clair
pour le MVP ; à chiffrer via DPAPI ensuite) et `dictae.sqlite` (table `history`).
