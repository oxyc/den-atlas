/**
 * The dataset artifacts Atlas serves: the two derived blobs (`labels-tNN.json` + `vectors-eNN.bin`, the
 * exact format the Den app parses) plus their metadata. Loaded from a data dir at startup (see
 * `scripts/import-dataset.mjs`, which imports them from the Den repo and writes `dataset.meta.json`).
 * sha256 + size are computed from the actual bytes here, so the served descriptor can never disagree with
 * the bytes on disk.
 */
import { readFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { join } from "node:path";

export interface DatasetMeta {
  datasetVersion: string;
  taxonomyVersion: string;
  embeddingModel: string;
  dims: number;
  count: number;
  quantization: string;
}

export interface DatasetBlob {
  /** Served path + descriptor filename, e.g. `labels-t01.json`. */
  name: string;
  bytes: Uint8Array;
  sha256: string;
  size: number;
  contentType: string;
}

export interface DatasetArtifacts {
  meta: DatasetMeta;
  labels: DatasetBlob;
  vectors: DatasetBlob;
}

interface DatasetMetaFile extends DatasetMeta {
  labelsFile: string;
  vectorsFile: string;
}

export function sha256Hex(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}

export function makeBlob(name: string, bytes: Uint8Array, contentType: string): DatasetBlob {
  return { name, bytes, sha256: sha256Hex(bytes), size: bytes.length, contentType };
}

/** Read `dataset.meta.json` + the two blobs from `dir`. Throws if anything is missing — a misconfigured
 * deploy should fail loudly at boot, not serve a half dataset. */
export async function loadDataset(dir: string): Promise<DatasetArtifacts> {
  const meta = JSON.parse(await readFile(join(dir, "dataset.meta.json"), "utf8")) as DatasetMetaFile;
  const labels = new Uint8Array(await readFile(join(dir, meta.labelsFile)));
  const vectors = new Uint8Array(await readFile(join(dir, meta.vectorsFile)));
  return {
    meta: {
      datasetVersion: meta.datasetVersion,
      taxonomyVersion: meta.taxonomyVersion,
      embeddingModel: meta.embeddingModel,
      dims: meta.dims,
      count: meta.count,
      quantization: meta.quantization,
    },
    labels: makeBlob(meta.labelsFile, labels, "application/json"),
    vectors: makeBlob(meta.vectorsFile, vectors, "application/octet-stream"),
  };
}
