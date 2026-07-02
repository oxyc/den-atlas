import { describe, it, expect } from "vitest";
import { isAppAcceptable, isSyncableUrl, type DatasetDescriptor } from "../src/descriptor.js";

const HEX64 = "a".repeat(64);
function descriptor(overrides: Partial<DatasetDescriptor> = {}): DatasetDescriptor {
  return {
    datasetVersion: "v1",
    taxonomyVersion: "t01",
    embeddingModel: "e02",
    dims: 384,
    count: 57872,
    quantization: "int8-symmetric-x127",
    labels: { url: "https://atlas.example/labels-t01.json", sha256: HEX64, bytes: 100 },
    vectors: { url: "https://atlas.example/vectors-e02.bin", sha256: HEX64, bytes: 200 },
    ...overrides,
  };
}

describe("isAppAcceptable mirrors the on-device DatasetDescriptor guards", () => {
  it("accepts a well-formed descriptor", () => {
    expect(isAppAcceptable(descriptor())).toBe(true);
  });

  it("rejects a public http blob URL (SSRF rule)", () => {
    expect(isAppAcceptable(descriptor({ labels: { url: "http://atlas.example/labels-t01.json", sha256: HEX64, bytes: 100 } }))).toBe(false);
  });

  it("rejects a non-hex / too-short checksum", () => {
    expect(isAppAcceptable(descriptor({ vectors: { url: "https://atlas.example/vectors-e02.bin", sha256: "xyz", bytes: 200 } }))).toBe(false);
  });

  it("rejects dims=0 and count=0", () => {
    expect(isAppAcceptable(descriptor({ dims: 0 }))).toBe(false);
    expect(isAppAcceptable(descriptor({ count: 0 }))).toBe(false);
  });

  it("rejects an unsafe (path-traversal) version token", () => {
    expect(isAppAcceptable(descriptor({ embeddingModel: "../../etc" }))).toBe(false);
    expect(isAppAcceptable(descriptor({ taxonomyVersion: "bad/slug" }))).toBe(false);
  });

  it("rejects a zero-byte blob", () => {
    expect(isAppAcceptable(descriptor({ labels: { url: "https://atlas.example/labels-t01.json", sha256: HEX64, bytes: 0 } }))).toBe(false);
  });
});

describe("isSyncableUrl", () => {
  it("allows https anywhere and http on LAN/localhost", () => {
    expect(isSyncableUrl("https://cdn.example/x.bin")).toBe(true);
    expect(isSyncableUrl("http://localhost:8080/x.bin")).toBe(true);
    expect(isSyncableUrl("http://192.168.1.10/x.bin")).toBe(true);
    expect(isSyncableUrl("http://10.0.0.5/x.bin")).toBe(true);
    expect(isSyncableUrl("http://172.16.4.4/x.bin")).toBe(true);
  });
  it("rejects public http and non-http schemes", () => {
    expect(isSyncableUrl("http://cdn.example/x.bin")).toBe(false);
    expect(isSyncableUrl("file:///etc/passwd")).toBe(false);
    expect(isSyncableUrl("not a url")).toBe(false);
  });
});
