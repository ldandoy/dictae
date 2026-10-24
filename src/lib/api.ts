import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Phase = "idle" | "recording" | "processing";
export type Mode = "push_to_talk" | "hands_free";

export interface DictationState {
  phase: Phase;
  mode: Mode;
  message: string | null;
  error: boolean;
  generation: number;
}

export type AsrProvider = "local" | "openai";

export interface Settings {
  asrProvider: AsrProvider;
  localModel: string;
  localThreads: number;
  streamingEnabled: boolean;
  asrBaseUrl: string;
  asrApiKey: string;
  asrModel: string;
  language: string;
  formatEnabled: boolean;
  ollamaBaseUrl: string;
  ollamaModel: string;
  formatInstructions: string;
  contextEnabled: boolean;
  shortcut: string;
  restoreClipboard: boolean;
  pasteDelayMs: number;
  handsFreeTapMs: number;
}

export interface ModelStatus {
  id: string;
  label: string;
  file: string;
  bytes: number;
  note: string;
  installed: boolean;
  downloading: boolean;
}

export interface ModelProgress {
  id: string;
  received: number;
  total: number;
  done: boolean;
  error: string | null;
}

export interface HistoryEntry {
  id: string;
  createdAt: string;
  asrText: string;
  formattedText: string | null;
  pastedText: string | null;
  status: string;
  durationMs: number;
  latencyMs: number;
  model: string;
  error: string | null;
  app: string | null;
  windowTitle: string | null;
  url: string | null;
}

export const api = {
  getSettings: () => invoke<Settings>("get_settings"),
  saveSettings: (settings: Settings) => invoke<void>("save_settings", { settings }),
  testAsr: () => invoke<string>("test_asr"),
  listLocalModels: () => invoke<ModelStatus[]>("list_local_models"),
  downloadLocalModel: (id: string) => invoke<void>("download_local_model", { id }),
  deleteLocalModel: (id: string) => invoke<void>("delete_local_model", { id }),
  listOllamaModels: (baseUrl?: string) => invoke<string[]>("list_ollama_models", { baseUrl }),
  testFormat: (sample?: string) => invoke<string>("test_format", { sample }),
  listHistory: (limit = 200) => invoke<HistoryEntry[]>("list_history", { limit }),
  deleteHistory: (id: string) => invoke<void>("delete_history", { id }),
  clearHistory: () => invoke<void>("clear_history"),
  getDictationState: () => invoke<DictationState>("get_dictation_state"),
  toggleDictation: () => invoke<void>("toggle_dictation"),
  cancelDictation: () => invoke<void>("cancel_dictation"),
  showHub: () => invoke<void>("show_hub"),
};

export const onDictationState = (cb: (s: DictationState) => void): Promise<UnlistenFn> =>
  listen<DictationState>("dictation://state", (e) => cb(e.payload));

export const onDictationLevel = (cb: (level: number) => void): Promise<UnlistenFn> =>
  listen<{ level: number }>("dictation://level", (e) => cb(e.payload.level));

export const onDictationPartial = (cb: (text: string) => void): Promise<UnlistenFn> =>
  listen<{ text: string }>("dictation://partial", (e) => cb(e.payload.text));

export const onDictationResult =(cb: (entry: HistoryEntry) => void): Promise<UnlistenFn> =>
  listen<{ entry: HistoryEntry }>("dictation://result", (e) => cb(e.payload.entry));

export const onSettingsChanged = (cb: (s: Settings) => void): Promise<UnlistenFn> =>
  listen<Settings>("settings://changed", (e) => cb(e.payload));

export const onModelProgress = (cb: (p: ModelProgress) => void): Promise<UnlistenFn> =>
  listen<ModelProgress>("models://progress", (e) => cb(e.payload));
