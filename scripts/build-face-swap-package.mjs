#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { createHash, createPrivateKey, sign as cryptoSign } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { canonicalJson } from "./lib/mlxPackage.mjs";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const OUTPUT = join(ROOT, "packaging/face-swap/dist");
const BINARY = join(OUTPUT, process.platform === "win32" ? "studio-tool-face-swap.exe" : "studio-tool-face-swap");
const VERSION = process.env.FACE_SWAP_VERSION ?? "ghost-3_256-0.2.0";
const COMPONENT_ID = "face-swap-ghost-3_256";
const SOURCE_ID = "little-monkey-face-swap";
const DATA_SEPARATOR = process.platform === "win32" ? ";" : ":";
const TARGET_OS = process.platform === "darwin" ? "macos" : process.platform === "win32" ? "windows" : process.platform;
const TARGET_ARCH = process.arch === "x64" ? "x86_64" : process.arch === "arm64" ? "aarch64" : process.arch;
const SIGNATURE_METADATA_KEYS = [
  "publisherSignatureAlgorithm",
  "publisherSignatureKeyId",
  "publisherSignatureBase64",
];

function requiredDirectory(name, value) {
  if (!value) throw new Error(`${name} is required`);
  const directory = resolve(value);
  if (!existsSync(directory) || !statSync(directory).isDirectory()) {
    throw new Error(`${name} is not a directory: ${directory}`);
  }
  return directory;
}

function requiredFile(name, value) {
  if (!value) throw new Error(`${name} is required`);
  const file = resolve(value);
  if (!existsSync(file) || !statSync(file).isFile()) {
    throw new Error(`${name} is not a file: ${file}`);
  }
  return file;
}

function sha256File(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

function signCatalogEntry(entry, privateKeyPem) {
  const key = createPrivateKey(privateKeyPem);
  if (key.asymmetricKeyType !== "ed25519") {
    throw new Error(`FACE_SWAP_SIGNING_KEY is ${key.asymmetricKeyType}, expected ed25519`);
  }
  const unsigned = {
    ...entry,
    metadata: { ...entry.metadata },
  };
  for (const name of SIGNATURE_METADATA_KEYS) delete unsigned.metadata[name];
  const signature = cryptoSign(null, canonicalJson(unsigned), key).toString("base64");
  return {
    ...entry,
    metadata: {
      ...entry.metadata,
      publisherSignatureAlgorithm: "ed25519",
      publisherSignatureKeyId: process.env.FACE_SWAP_SIGNING_KEY_ID ?? "release-2026-1",
      publisherSignatureBase64: signature,
    },
  };
}

function run() {
  const downloadUrl = process.env.FACE_SWAP_DOWNLOAD_URL;
  if (downloadUrl && !downloadUrl.startsWith("https://")) {
    throw new Error("FACE_SWAP_DOWNLOAD_URL must use HTTPS for Runtime Hub catalogs");
  }
  const modelRoot = requiredDirectory("FACE_SWAP_MODEL_ROOT", process.env.FACE_SWAP_MODEL_ROOT);
  const modelLicense = requiredFile(
    "FACE_SWAP_MODEL_LICENSE_FILE",
    process.env.FACE_SWAP_MODEL_LICENSE_FILE,
  );
  const packName = process.env.FACE_SWAP_FACE_PACK ?? "buffalo_l";
  for (const file of ["det_10g.onnx", "w600k_r50.onnx", "genderage.onnx"]) {
    requiredFile(`face-analysis pack ${packName}/${file}`, join(modelRoot, "models", packName, file));
  }
  const ghostModel = requiredFile(
    "FACE_SWAP_MODEL",
    process.env.FACE_SWAP_MODEL ?? join(modelRoot, "models/ghost_3_256.onnx"),
  );
  const embeddingConverter = requiredFile(
    "FACE_SWAP_EMBEDDING_CONVERTER",
    process.env.FACE_SWAP_EMBEDDING_CONVERTER ?? join(modelRoot, "models/crossface_ghost.onnx"),
  );
  if (relative(modelRoot, ghostModel).startsWith("..") || relative(modelRoot, embeddingConverter).startsWith("..")) {
    throw new Error("GHOST model files must be inside FACE_SWAP_MODEL_ROOT so they can be bundled");
  }
  const codeformerModelValue = process.env.FACE_SWAP_CODEFORMER_MODEL;
  const codeformerHomeValue = process.env.FACE_SWAP_CODEFORMER_HOME;
  if (Boolean(codeformerModelValue) !== Boolean(codeformerHomeValue)) {
    throw new Error("FACE_SWAP_CODEFORMER_MODEL and FACE_SWAP_CODEFORMER_HOME must be provided together");
  }
  const codeformerModel = codeformerModelValue
    ? requiredFile("FACE_SWAP_CODEFORMER_MODEL", codeformerModelValue)
    : null;
  const codeformerHome = codeformerHomeValue
    ? requiredDirectory("FACE_SWAP_CODEFORMER_HOME", codeformerHomeValue)
    : null;
  const codeformerLicense = codeformerModel
    ? requiredFile("FACE_SWAP_CODEFORMER_LICENSE_FILE", process.env.FACE_SWAP_CODEFORMER_LICENSE_FILE)
    : null;
  if (codeformerModel && relative(modelRoot, codeformerModel).startsWith("..")) {
    throw new Error("FACE_SWAP_CODEFORMER_MODEL must be inside FACE_SWAP_MODEL_ROOT so it can be bundled");
  }
  if (codeformerHome && !existsSync(join(codeformerHome, "basicsr"))) {
    throw new Error(`FACE_SWAP_CODEFORMER_HOME is missing basicsr/: ${codeformerHome}`);
  }
  const configuredPython = process.env.FACE_SWAP_PYTHON;
  const python = configuredPython
    ? requiredFile("FACE_SWAP_PYTHON", configuredPython)
    : process.platform === "win32"
      ? "python"
      : "python3";
  rmSync(OUTPUT, { recursive: true, force: true });
  mkdirSync(OUTPUT, { recursive: true });
  execFileSync(
    python,
    [
      "-m",
      "PyInstaller",
      "--clean",
      "--noconfirm",
      "--onefile",
      "--name",
      "studio-tool-face-swap",
      "--distpath",
      OUTPUT,
      "--workpath",
      join(OUTPUT, ".pyinstaller-work"),
      "--specpath",
      OUTPUT,
      "--hidden-import",
      "insightface.app",
      "--hidden-import",
      "insightface.model_zoo",
      "--hidden-import",
      "insightface.model_zoo.model_zoo",
      "--collect-submodules",
      "insightface",
      "--add-data",
      `${join(modelRoot, "models", packName)}${DATA_SEPARATOR}face-swap-models/models/${packName}`,
      "--add-data",
      `${ghostModel}${DATA_SEPARATOR}face-swap-models/models/ghost_3_256.onnx`,
      "--add-data",
      `${embeddingConverter}${DATA_SEPARATOR}face-swap-models/models/crossface_ghost.onnx`,
      "--add-data",
      `${modelLicense}${DATA_SEPARATOR}licenses/model-license.txt`,
      ...(codeformerHome
        ? [
            "--paths",
            codeformerHome,
            "--hidden-import",
            "torch",
            "--hidden-import",
            "torchvision",
            "--collect-all",
            "torch",
            "--collect-all",
            "torchvision",
            "--collect-submodules",
            "basicsr",
            "--hidden-import",
            "basicsr.archs.codeformer_arch",
            "--hidden-import",
            "basicsr.archs.vqgan_arch",
            "--add-data",
            `${codeformerHome}${DATA_SEPARATOR}codeformer-source`,
          ]
        : []),
      ...(codeformerLicense
        ? ["--add-data", `${codeformerLicense}${DATA_SEPARATOR}licenses/codeformer-license.txt`]
        : []),
      ...(codeformerModel
        ? ["--add-data", `${codeformerModel}${DATA_SEPARATOR}face-swap-models/models/codeformer.pth`]
        : []),
      join(ROOT, "studio-tools/face-swap/studio-tool-face-swap.py"),
    ],
    { stdio: "inherit" },
  );

  if (!existsSync(BINARY)) throw new Error(`PyInstaller did not produce ${BINARY}`);
  const bytes = readFileSync(BINARY);
  let catalog = [
    {
      schemaVersion: 1,
      sourceId: SOURCE_ID,
      componentId: COMPONENT_ID,
      kind: "studio_tool",
      displayName: "Face Swap (GHOST 3_256)",
      accelerator: null,
      version: VERSION,
      channel: "pinned",
      downloadUrl: downloadUrl ?? "",
      sha256: createHash("sha256").update(bytes).digest("hex"),
      sizeBytes: bytes.length,
      publishedAtMs: Number(process.env.SOURCE_DATE_EPOCH ?? 0) * 1000,
      compatibilityNote: "NON-COMMERCIAL USE ONLY: public InsightFace weights are licensed for non-commercial academic research. Separate permission is required for commercial use.",
      metadata: {
        engine: "ghost-3_256",
        modelLicenseFile: "licenses/model-license.txt",
        sourceRepository: "https://github.com/ai-forever/ghost",
        embeddingConverter: "crossface_ghost.onnx",
        faceAnalysisDetectorSha256: sha256File(join(modelRoot, "models", packName, "det_10g.onnx")),
        faceAnalysisRecognitionSha256: sha256File(join(modelRoot, "models", packName, "w600k_r50.onnx")),
        faceAnalysisGenderageSha256: sha256File(join(modelRoot, "models", packName, "genderage.onnx")),
        ghostModelSha256: sha256File(ghostModel),
        embeddingConverterSha256: sha256File(embeddingConverter),
        codeformerSha256: codeformerModel ? sha256File(codeformerModel) : "none",
        codeformerLicenseFile: codeformerLicense ? "licenses/codeformer-license.txt" : "none",
        targetOs: TARGET_OS,
        targetArch: TARGET_ARCH,
      },
    },
  ];
  const catalogPath = join(ROOT, "packaging/face-swap/face-swap-catalog.json");
  if (downloadUrl) {
    const signingKey = process.env.FACE_SWAP_SIGNING_KEY;
    if (!signingKey) {
      throw new Error("FACE_SWAP_SIGNING_KEY is required when publishing a managed catalog");
    }
    catalog = catalog.map((entry) => signCatalogEntry(entry, signingKey));
    mkdirSync(dirname(catalogPath), { recursive: true });
    writeFileSync(catalogPath, `${JSON.stringify(catalog, null, 2)}\n`);
  }
  console.log(`binary: ${BINARY} (${(bytes.length / 1e6).toFixed(0)} MB)`);
  console.log(downloadUrl ? `catalog: ${catalogPath}` : "catalog: skipped (set FACE_SWAP_DOWNLOAD_URL to publish)");
}

run();
