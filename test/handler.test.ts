import { describe, it, expect } from "vitest";
import { handleAtlas } from "../src/handler.js";
import { makeBlob, type DatasetArtifacts } from "../src/dataset.js";
import { isAppAcceptable, type DatasetDescriptor } from "../src/descriptor.js";

/** A tiny in-memory dataset (real format, small bytes) — the handler never touches the filesystem. */
function fakeDataset(): DatasetArtifacts {
  const labelsJson = JSON.stringify({ taxonomyVersion: "t01", count: 2, records: [] });
  const labels = new TextEncoder().encode(labelsJson);
  // vectors: [int32 count=2][int32 dim=4] + 2×4 int8 rows
  const vectors = new Uint8Array([2, 0, 0, 0, 4, 0, 0, 0, 10, 20, 30, 40, 50, 60, 70, 80]);
  return {
    meta: { datasetVersion: "abc123def456", taxonomyVersion: "t01", embeddingModel: "e02", dims: 4, count: 2, quantization: "int8-symmetric-x127" },
    labels: makeBlob("labels-t01.json", labels, "application/json"),
    vectors: makeBlob("vectors-e02.bin", vectors, "application/octet-stream"),
  };
}

const dataset = fakeDataset();
function req(path: string, headers: Record<string, string> = {}, method = "GET") {
  return new Request(`http://internal.local${path}`, { headers, method });
}
const proxied = { "x-forwarded-proto": "https", "x-forwarded-host": "atlas.example" };

describe("manifest", () => {
  it("declares the dataset resource for movie + series", async () => {
    const res = handleAtlas(req("/manifest.json"), { dataset });
    const m = (await res.json()) as Record<string, any>;
    expect(m.id).toBe("com.den.atlas");
    expect(m.resources).toContain("dataset");
    expect(m.types).toEqual(["movie", "series"]);
    expect(m.behaviorHints.configurationRequired).toBe(false);
  });
});

describe("dataset.json descriptor", () => {
  it("builds absolute blob URLs from the forwarded origin and is app-acceptable", async () => {
    const res = handleAtlas(req("/dataset.json", proxied), { dataset });
    expect(res.status).toBe(200);
    const d = (await res.json()) as DatasetDescriptor;
    expect(d.labels.url).toBe("https://atlas.example/labels-t01.json?v=abc123def456");
    expect(d.vectors.url).toBe("https://atlas.example/vectors-e02.bin?v=abc123def456");
    expect(d.dims).toBe(4);
    expect(d.count).toBe(2);
    expect(d.embeddingModel).toBe("e02");
    expect(d.labels.sha256).toBe(dataset.labels.sha256);
    expect(d.labels.bytes).toBe(dataset.labels.size);
    // The whole point: what Atlas emits must decode + validate on-device.
    expect(isAppAcceptable(d)).toBe(true);
  });

  it("honors PUBLIC_BASE_URL override", async () => {
    const res = handleAtlas(req("/dataset.json", proxied), { dataset, publicBaseUrl: "https://cdn.example/atlas" });
    const d = (await res.json()) as DatasetDescriptor;
    expect(d.vectors.url).toBe("https://cdn.example/atlas/vectors-e02.bin?v=abc123def456");
  });

  it("dataset.json 304s on a matching If-None-Match", () => {
    const first = handleAtlas(req("/dataset.json", proxied), { dataset });
    const etag = first.headers.get("etag")!;
    const second = handleAtlas(req("/dataset.json", { ...proxied, "if-none-match": etag }), { dataset });
    expect(second.status).toBe(304);
  });
});

describe("blob serving", () => {
  it("serves the labels blob with a sha256 ETag", async () => {
    const res = handleAtlas(req("/labels-t01.json"), { dataset });
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toBe("application/json");
    expect(res.headers.get("etag")).toBe(`"${dataset.labels.sha256}"`);
    const body = new Uint8Array(await res.arrayBuffer());
    expect(body).toEqual(dataset.labels.bytes);
  });

  it("serves the vectors blob as octet-stream", async () => {
    const res = handleAtlas(req("/vectors-e02.bin"), { dataset });
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toBe("application/octet-stream");
    expect(res.headers.get("content-length")).toBe(String(dataset.vectors.size));
  });

  it("returns 304 on a matching If-None-Match", async () => {
    const etag = `"${dataset.vectors.sha256}"`;
    const res = handleAtlas(req("/vectors-e02.bin", { "if-none-match": etag }), { dataset });
    expect(res.status).toBe(304);
  });

  it("is immutable when the version query matches, revalidatable otherwise", () => {
    const pinned = handleAtlas(req("/vectors-e02.bin?v=abc123def456"), { dataset });
    expect(pinned.headers.get("cache-control")).toContain("immutable");
    const bare = handleAtlas(req("/vectors-e02.bin"), { dataset });
    expect(bare.headers.get("cache-control")).toBe("public, max-age=3600");
  });

  it("HEAD returns headers with no body", async () => {
    const res = handleAtlas(req("/vectors-e02.bin", {}, "HEAD"), { dataset });
    expect(res.status).toBe(200);
    expect(res.headers.get("content-length")).toBe(String(dataset.vectors.size));
    expect((await res.arrayBuffer()).byteLength).toBe(0);
  });
});

describe("method handling", () => {
  it("rejects non-GET/HEAD with 405", () => {
    expect(handleAtlas(req("/manifest.json", {}, "POST"), { dataset }).status).toBe(405);
  });
});

describe("misc routes", () => {
  it("health is ok", async () => {
    const body = (await handleAtlas(req("/health"), { dataset }).json()) as Record<string, unknown>;
    expect(body.status).toBe("ok");
  });
  it("landing page renders with the install URL", async () => {
    const res = handleAtlas(req("/", proxied), { dataset });
    expect(res.headers.get("content-type")).toContain("text/html");
    expect(await res.text()).toContain("https://atlas.example/manifest.json");
  });
  it("unknown path is 404", async () => {
    expect(handleAtlas(req("/nope"), { dataset }).status).toBe(404);
  });
});
