/**
 * The dataset artifacts Atlas serves: the two derived blobs (`labels-tNN.json` + `vectors-eNN.bin`, the
 * exact format the Den app parses) plus their metadata. Loaded from a data dir at startup (see
 * `scripts/import-dataset.mjs`, which imports them from the Den repo and writes `dataset.meta.json`).
 * sha256 + size are computed from the actual bytes here, so the served descriptor can never disagree with
 * the bytes on disk.
 */
import { readFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { gzipSync } from "node:zlib";
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
  /** Precomputed gzip of `bytes`, where compression pays (the labels JSON). Undefined → serve identity. */
  gzip?: Uint8Array;
}

export interface DatasetArtifacts {
  meta: DatasetMeta;
  labels: DatasetBlob;
  vectors: DatasetBlob;
  /** When the artifacts were built (HTTP-date), for `Last-Modified`. Undefined if the meta predates it. */
  lastModified?: string;
}

interface DatasetMetaFile extends DatasetMeta {
  labelsFile: string;
  vectorsFile: string;
  builtAt?: string; // ISO-8601, written by the import script
}

export function sha256Hex(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}

export function makeBlob(name: string, bytes: Uint8Array, contentType: string, gzip?: Uint8Array): DatasetBlob {
  return { name, bytes, sha256: sha256Hex(bytes), size: bytes.length, contentType, gzip };
}

/** Read `dataset.meta.json` + the two blobs from `dir`. Throws if anything is missing — a misconfigured
 * deploy should fail loudly at boot, not serve a half dataset. The labels JSON is gzipped once here (it
 * compresses ~4×); the int8 vectors are near-incompressible, so they're served identity. */
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
    labels: makeBlob(meta.labelsFile, labels, "application/json", new Uint8Array(gzipSync(labels))),
    vectors: makeBlob(meta.vectorsFile, vectors, "application/octet-stream"),
    lastModified: meta.builtAt ? new Date(meta.builtAt).toUTCString() : undefined,
  };
}
