// Récupère le binaire llama-server (llama.cpp, build Vulkan Windows x64) et ses DLL dans
// src-tauri/resources/llama/, pour le développement comme pour le build (CI).
// Idempotent : ne retélécharge pas si la version attendue est déjà en place.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";

const LLAMA_BUILD = "b11873";
const ZIP_NAME = `llama-${LLAMA_BUILD}-bin-win-vulkan-x64.zip`;
const URL = `https://github.com/ggml-org/llama.cpp/releases/download/${LLAMA_BUILD}/${ZIP_NAME}`;

const here = path.dirname(fileURLToPath(import.meta.url));
const dest = path.resolve(here, "..", "src-tauri", "resources", "llama");
const stamp = path.join(dest, "VERSION");

if (process.platform !== "win32") {
  console.log("fetch-llama : plateforme non Windows, rien à faire");
  process.exit(0);
}
if (fs.existsSync(stamp) && fs.readFileSync(stamp, "utf8").trim() === LLAMA_BUILD && fs.existsSync(path.join(dest, "llama-server.exe"))) {
  console.log(`fetch-llama : llama.cpp ${LLAMA_BUILD} déjà présent dans ${dest}`);
  process.exit(0);
}

fs.rmSync(dest, { recursive: true, force: true });
fs.mkdirSync(dest, { recursive: true });
const zipPath = path.join(dest, ZIP_NAME);
console.log(`fetch-llama : téléchargement de ${URL}`);
const res = await fetch(URL, { redirect: "follow" });
if (!res.ok) throw new Error(`téléchargement impossible : HTTP ${res.status}`);
fs.writeFileSync(zipPath, Buffer.from(await res.arrayBuffer()));

// Extraction avec PowerShell (pas de dépendance npm), puis on ne garde que l'utile.
const extractDir = path.join(dest, "_extract");
execFileSync("powershell.exe", [
  "-NoProfile",
  "-Command",
  `Expand-Archive -LiteralPath '${zipPath}' -DestinationPath '${extractDir}' -Force`,
]);
// llama-server.exe n'est qu'un lanceur : le serveur est dans llama-server-impl.dll.
const wanted = /^(llama-server\.exe|llama-server-impl\.dll|llama-common\.dll|llama\.dll|libomp\.dll|ggml.*\.dll|mtmd\.dll|LICENSE.*)$/i;
const walk = (dir) => {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(p);
    else if (wanted.test(entry.name)) fs.copyFileSync(p, path.join(dest, entry.name));
  }
};
walk(extractDir);
fs.rmSync(extractDir, { recursive: true, force: true });
fs.rmSync(zipPath, { force: true });
fs.writeFileSync(stamp, LLAMA_BUILD + "\n");
console.log(`fetch-llama : ${fs.readdirSync(dest).join(", ")}`);
