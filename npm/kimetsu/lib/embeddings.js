"use strict";

// On-demand embeddings build fetcher.
//
// optionalDependencies can only be selected by npm from os/cpu — it cannot be
// driven by an env var. So the lean build ships as the platform package, and
// the (larger) embeddings build is fetched on first use when the user opts in
// with KIMETSU_NPM_FLAVOR=embeddings.
//
// This mirrors the asset naming + download/extract flow in
// crates/kimetsu-cli/src/update.rs (download_asset / extract_binary) and the
// archive layout produced by .github/workflows/release.yml (an archive whose
// single top-level dir is `kimetsu-<version>-<target>-embeddings/`).

const fs = require("fs");
const os = require("os");
const path = require("path");
const https = require("https");
const crypto = require("crypto");
const { execFileSync } = require("child_process");

const REPO = "RodCor/kimetsu";

function cacheRoot() {
  if (process.platform === "win32") {
    return process.env.LOCALAPPDATA || path.join(os.homedir(), "AppData", "Local");
  }
  if (process.platform === "darwin") {
    return path.join(os.homedir(), "Library", "Caches");
  }
  return process.env.XDG_CACHE_HOME || path.join(os.homedir(), ".cache");
}

// Persisted flavor preference, so the choice survives across runs without an
// env var. `kimetsu npm-flavor embeddings|lean` writes this; the launcher reads
// it when KIMETSU_NPM_FLAVOR isn't set.
function flavorMarkerPath() {
  return path.join(cacheRoot(), "kimetsu", "npm", "flavor");
}

function readFlavorMarker() {
  try {
    const v = fs.readFileSync(flavorMarkerPath(), "utf8").trim().toLowerCase();
    return v === "embeddings" || v === "lean" ? v : null;
  } catch (_err) {
    return null;
  }
}

function writeFlavorMarker(flavor) {
  const p = flavorMarkerPath();
  fs.mkdirSync(path.dirname(p), { recursive: true });
  fs.writeFileSync(p, flavor + "\n");
}

function downloadUrl(version, assetName) {
  return `https://github.com/${REPO}/releases/download/v${version}/${assetName}`;
}

// GitHub release downloads redirect (to objects.githubusercontent.com / S3).
function download(url, dest, redirectsLeft = 5) {
  return new Promise((resolve, reject) => {
    const req = https.get(
      url,
      {
        headers: {
          Accept: "application/octet-stream",
          "User-Agent": `kimetsu-npm/${require("../package.json").version}`,
        },
      },
      (res) => {
        if (
          res.statusCode &&
          res.statusCode >= 300 &&
          res.statusCode < 400 &&
          res.headers.location
        ) {
          res.resume();
          if (redirectsLeft <= 0) {
            reject(new Error("too many redirects"));
            return;
          }
          resolve(download(res.headers.location, dest, redirectsLeft - 1));
          return;
        }
        if (res.statusCode !== 200) {
          res.resume();
          reject(new Error(`download failed: HTTP ${res.statusCode} for ${url}`));
          return;
        }
        const file = fs.createWriteStream(dest);
        res.pipe(file);
        file.on("finish", () => file.close((err) => (err ? reject(err) : resolve())));
        file.on("error", reject);
      }
    );
    req.on("error", reject);
  });
}

// bsdtar (`tar`) is present on Linux, macOS, and Windows 10 1803+, and handles
// both .tar.gz and .zip — so we shell out instead of bundling a tar/unzip dep.
function extract(archive, dest) {
  fs.mkdirSync(dest, { recursive: true });
  execFileSync("tar", ["-xf", archive, "-C", dest], { stdio: "ignore" });
}

function sha256File(filePath) {
  return new Promise((resolve, reject) => {
    const hash = crypto.createHash("sha256");
    const stream = fs.createReadStream(filePath);
    stream.on("data", (chunk) => hash.update(chunk));
    stream.on("error", reject);
    stream.on("end", () => resolve(hash.digest("hex")));
  });
}

function checksumForAsset(manifest, assetName) {
  for (const rawLine of manifest.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    let match = line.match(/^([a-fA-F0-9]{64})\s+\*?(.+)$/);
    if (match && match[2] === assetName) return match[1].toLowerCase();
    match = line.match(/^SHA256 \((.+)\) = ([a-fA-F0-9]{64})$/);
    if (match && match[1] === assetName) return match[2].toLowerCase();
  }
  return null;
}

async function verifyChecksum(archivePath, assetName, checksumsPath) {
  const manifest = fs.readFileSync(checksumsPath, "utf8");
  const expected = checksumForAsset(manifest, assetName);
  if (!expected) {
    throw new Error(`checksums.txt does not contain ${assetName}`);
  }
  const actual = await sha256File(archivePath);
  if (actual !== expected) {
    throw new Error(`checksum mismatch for ${assetName}: expected ${expected}, got ${actual}`);
  }
}

async function ensureEmbeddingsBinary({ version, target, binName }) {
  const cacheDir = path.join(
    cacheRoot(),
    "kimetsu",
    "npm",
    `v${version}`,
    `${target}-embeddings`
  );
  const cachedBin = path.join(cacheDir, binName);
  if (fs.existsSync(cachedBin)) return cachedBin;

  const ext = process.platform === "win32" ? "zip" : "tar.gz";
  const stem = `kimetsu-${version}-${target}-embeddings`;
  const assetName = `${stem}.${ext}`;

  const workdir = fs.mkdtempSync(path.join(os.tmpdir(), "kimetsu-npm-"));
  try {
    const archivePath = path.join(workdir, assetName);
    const checksumsPath = path.join(workdir, "checksums.txt");
    process.stderr.write(`kimetsu: fetching embeddings build (${assetName})…\n`);
    await download(downloadUrl(version, assetName), archivePath);
    await download(downloadUrl(version, "checksums.txt"), checksumsPath);
    await verifyChecksum(archivePath, assetName, checksumsPath);

    const extractDir = path.join(workdir, "extract");
    extract(archivePath, extractDir);

    // Archives wrap the binary in a top-level `<stem>/` directory.
    const candidates = [
      path.join(extractDir, stem, binName),
      path.join(extractDir, binName),
    ];
    const extracted = candidates.find((p) => fs.existsSync(p));
    if (!extracted) {
      throw new Error(`binary ${binName} not found inside ${assetName}`);
    }

    fs.mkdirSync(cacheDir, { recursive: true });
    fs.copyFileSync(extracted, cachedBin);
    if (process.platform !== "win32") fs.chmodSync(cachedBin, 0o755);
    return cachedBin;
  } finally {
    try {
      fs.rmSync(workdir, { recursive: true, force: true });
    } catch (_err) {
      /* best effort */
    }
  }
}

module.exports = {
  ensureEmbeddingsBinary,
  readFlavorMarker,
  writeFlavorMarker,
  flavorMarkerPath,
};
