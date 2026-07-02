/**
 * Import the derived dataset artifacts from the Den repo into ./data and write dataset.meta.json.
 *
 *   DEN_REPO=/path/to/den npm run import          (defaults to ../den next to this repo)
 *
 * The blobs (labels-tNN.json + vectors-eNN.bin) are the SAME format the Den app parses; Atlas just serves
 * them. They're gitignored — re-run this to refresh, then rebuild the Docker image. `dims`/`count` are read
 * from the vectors header + labels JSON (never hand-typed), and `datasetVersion` is content-addressed
 * (first 12 hex of sha256(labelsSha:vectorsSha)) so it changes iff the data changes → the app re-syncs
 * exactly when it should, and not otherwise.
 */
import { readFile, writeFile, copyFile, mkdir } from "node:fs/promises";
import { createHash } from "node:crypto";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "..");
const denRepo = process.env.DEN_REPO ?? resolve(repoRoot, "..", "den");
const resources = join(denRepo, "Sources", "DenKit", "Resources");
const dataDir = join(repoRoot, "data");

// The artifacts shipped today. Bump these when a new taxonomy/embedding version is published.
const TAXONOMY_VERSION = process.env.ATLAS_TAXONOMY ?? "t01";
const EMBEDDING_MODEL = process.env.ATLAS_EMBEDDING ?? "e02";
const labelsFile = `labels-${TAXONOMY_VERSION}.json`;
const vectorsFile = `vectors-${EMBEDDING_MODEL}.bin`;

function sha256(buf) {
  return createHash("sha256").update(buf).digest("hex");
}

const labelsPath = join(resources, labelsFile);
const vectorsPath = join(resources, vectorsFile);

const labels = await readFile(labelsPath);
const vectors = await readFile(vectorsPath);

// Vectors header: little-endian [int32 count][int32 dim].
const count = vectors.readInt32LE(0);
const dims = vectors.readInt32LE(4);
const expectedBytes = 8 + count * dims;
if (vectors.length !== expectedBytes) {
  throw new Error(`vectors blob malformed: header says ${count}×${dims} (=${expectedBytes} bytes) but file is ${vectors.length}`);
}

const labelsJson = JSON.parse(labels.toString("utf8"));
if (labelsJson.taxonomyVersion !== TAXONOMY_VERSION) {
  throw new Error(`labels taxonomyVersion ${labelsJson.taxonomyVersion} ≠ expected ${TAXONOMY_VERSION}`);
}
if (labelsJson.count !== count) {
  throw new Error(`count mismatch: labels ${labelsJson.count} vs vectors ${count} — blobs are not aligned`);
}

const datasetVersion =
  process.env.ATLAS_DATASET_VERSION ?? sha256(`${sha256(labels)}:${sha256(vectors)}`).slice(0, 12);

await mkdir(dataDir, { recursive: true });
await copyFile(labelsPath, join(dataDir, labelsFile));
await copyFile(vectorsPath, join(dataDir, vectorsFile));

const meta = {
  datasetVersion,
  taxonomyVersion: TAXONOMY_VERSION,
  embeddingModel: EMBEDDING_MODEL,
  dims,
  count,
  quantization: "int8-symmetric-x127",
  labelsFile,
  vectorsFile,
};
await writeFile(join(dataDir, "dataset.meta.json"), JSON.stringify(meta, null, 2) + "\n");

console.log(
  `imported ${count} titles ×${dims}d — ${labelsFile} (${labels.length}B) + ${vectorsFile} (${vectors.length}B)\n` +
    `datasetVersion ${datasetVersion}`,
);
