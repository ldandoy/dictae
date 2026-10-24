// Bouton de téléchargement : dernière release GitHub.
// Le dépôt est déduit de l'URL GitHub Pages (owner.github.io/repo) ; sinon, FALLBACK_REPO.
(function () {
  const FALLBACK_REPO = "overconsulting/dictae";

  function detectRepo() {
    const host = location.hostname.toLowerCase();
    const m = host.match(/^([a-z0-9-]+)\.github\.io$/);
    const path = location.pathname.split("/").filter(Boolean);
    if (m && path.length > 0) return `${m[1]}/${path[0]}`;
    return FALLBACK_REPO;
  }

  const repo = detectRepo();
  const repoUrl = `https://github.com/${repo}`;
  const releasesUrl = `${repoUrl}/releases`;

  for (const id of ["nav-repo", "footer-repo"]) {
    const a = document.getElementById(id);
    if (a) a.href = repoUrl;
  }
  const releases = document.getElementById("releases");
  const download = document.getElementById("download");
  const meta = document.getElementById("download-meta");
  releases.href = releasesUrl;
  download.href = `${releasesUrl}/latest`;

  fetch(`https://api.github.com/repos/${repo}/releases/latest`, { headers: { Accept: "application/vnd.github+json" } })
    .then((r) => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
    .then((release) => {
      const assets = release.assets || [];
      const setup = assets.find((a) => /-setup\.exe$/i.test(a.name)) || assets.find((a) => /\.msi$/i.test(a.name));
      if (setup) {
        download.href = setup.browser_download_url;
        const mb = (setup.size / 1048576).toFixed(1).replace(".", ",");
        meta.textContent = `${release.tag_name} · ${mb} Mo`;
      } else {
        meta.textContent = `${release.tag_name} · voir les fichiers`;
      }
    })
    .catch(() => {
      meta.textContent = "dernière version sur GitHub";
    });
})();
