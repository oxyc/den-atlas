import { describe, it, expect } from "vitest";
import { gzipSync, gunzipSync } from "node:zlib";
import { serveBytes, parseRange, isNotModified, type Servable } from "../src/http.js";

const raw = new TextEncoder().encode("x".repeat(1000) + "payload");
const SHA = "a".repeat(64);
const LAST_MODIFIED = "Wed, 01 Jul 2026 00:00:00 GMT";

function servable(over: Partial<Servable> = {}): Servable {
  return {
    bytes: raw,
    etag: SHA,
    contentType: "application/octet-stream",
    cacheControl: "public, max-age=3600",
    lastModified: LAST_MODIFIED,
    ...over,
  };
}
function req(headers: Record<string, string> = {}, method = "GET") {
  return new Request("http://a.local/blob", { headers, method });
}

describe("conditional requests → 304", () => {
  it("304 on matching If-None-Match (and echoes validators, no body)", async () => {
    const res = serveBytes(req({ "if-none-match": `"${SHA}"` }), servable());
    expect(res.status).toBe(304);
    expect(res.headers.get("etag")).toBe(`"${SHA}"`);
    expect(res.headers.get("cache-control")).toBe("public, max-age=3600");
    expect((await res.arrayBuffer()).byteLength).toBe(0);
  });

  it("304 on If-None-Match: *", () => {
    expect(serveBytes(req({ "if-none-match": "*" }), servable()).status).toBe(304);
  });

  it("200 when If-None-Match does not match", () => {
    expect(serveBytes(req({ "if-none-match": '"deadbeef"' }), servable()).status).toBe(200);
  });

  it("304 on If-Modified-Since at/after the build time", () => {
    expect(serveBytes(req({ "if-modified-since": LAST_MODIFIED }), servable()).status).toBe(304);
    expect(serveBytes(req({ "if-modified-since": "Thu, 02 Jul 2026 00:00:00 GMT" }), servable()).status).toBe(304);
  });

  it("200 on If-Modified-Since before the build time", () => {
    expect(serveBytes(req({ "if-modified-since": "Tue, 30 Jun 2026 00:00:00 GMT" }), servable()).status).toBe(200);
  });

  it("If-None-Match takes precedence over If-Modified-Since", () => {
    // ETag mismatch ⇒ 200, even though the date says not-modified.
    const res = serveBytes(req({ "if-none-match": '"nope"', "if-modified-since": LAST_MODIFIED }), servable());
    expect(res.status).toBe(200);
    expect(isNotModified(req({ "if-none-match": '"nope"', "if-modified-since": LAST_MODIFIED }), `"${SHA}"`, LAST_MODIFIED)).toBe(false);
  });
});

describe("gzip negotiation", () => {
  const withGzip = () => servable({ gzip: new Uint8Array(gzipSync(raw)), contentType: "application/json" });

  it("serves gzip when accepted; body gunzips to the identity bytes; ETag is the distinct gzip variant", async () => {
    const res = serveBytes(req({ "accept-encoding": "gzip, deflate" }), withGzip());
    expect(res.headers.get("content-encoding")).toBe("gzip");
    expect(res.headers.get("vary")).toBe("Accept-Encoding");
    expect(res.headers.get("etag")).toBe(`"${SHA}-gzip"`); // distinct per content-coding (RFC 9110 §8.8.3)
    const body = new Uint8Array(await res.arrayBuffer());
    expect(new Uint8Array(gunzipSync(body))).toEqual(raw); // sha of the DECOMPRESSED bytes is what the app validates
  });

  it("304s a gzip client on its own ETag; the identity ETag does not match the gzip representation", () => {
    const gz = withGzip();
    expect(serveBytes(req({ "accept-encoding": "gzip", "if-none-match": `"${SHA}-gzip"` }), gz).status).toBe(304);
    expect(serveBytes(req({ "accept-encoding": "gzip", "if-none-match": `"${SHA}"` }), gz).status).toBe(200);
  });

  it("serves identity when gzip is not accepted or refused (q=0)", () => {
    expect(serveBytes(req({ "accept-encoding": "identity" }), withGzip()).headers.get("content-encoding")).toBeNull();
    expect(serveBytes(req({ "accept-encoding": "gzip;q=0" }), withGzip()).headers.get("content-encoding")).toBeNull();
    expect(serveBytes(req(), withGzip()).headers.get("content-encoding")).toBeNull();
  });

  it("treats Accept-Encoding: * as gzip-acceptable (but honors *;q=0)", () => {
    expect(serveBytes(req({ "accept-encoding": "*" }), withGzip()).headers.get("content-encoding")).toBe("gzip");
    expect(serveBytes(req({ "accept-encoding": "*;q=0" }), withGzip()).headers.get("content-encoding")).toBeNull();
  });

  it("sets Vary only when a gzip variant exists (so a CDN doesn't split-cache identity blobs)", () => {
    expect(serveBytes(req(), withGzip()).headers.get("vary")).toBe("Accept-Encoding");
    expect(serveBytes(req(), servable()).headers.get("vary")).toBeNull(); // identity-only → no Vary
  });
});

describe("range requests", () => {
  it("206 with Content-Range for a bounded range", async () => {
    const res = serveBytes(req({ range: "bytes=0-9" }), servable());
    expect(res.status).toBe(206);
    expect(res.headers.get("content-range")).toBe(`bytes 0-9/${raw.length}`);
    expect(res.headers.get("content-length")).toBe("10");
    expect(new Uint8Array(await res.arrayBuffer())).toEqual(raw.subarray(0, 10));
  });

  it("206 for an open-ended and a suffix range", () => {
    expect(serveBytes(req({ range: "bytes=1000-" }), servable()).headers.get("content-range")).toBe(`bytes 1000-${raw.length - 1}/${raw.length}`);
    expect(serveBytes(req({ range: "bytes=-7" }), servable()).headers.get("content-range")).toBe(`bytes ${raw.length - 7}-${raw.length - 1}/${raw.length}`);
  });

  it("416 for an unsatisfiable range", () => {
    const res = serveBytes(req({ range: `bytes=${raw.length + 10}-` }), servable());
    expect(res.status).toBe(416);
    expect(res.headers.get("content-range")).toBe(`bytes */${raw.length}`);
  });

  it("range wins over gzip (byte offsets need the identity representation)", () => {
    const res = serveBytes(req({ range: "bytes=0-9", "accept-encoding": "gzip" }), servable({ gzip: new Uint8Array(gzipSync(raw)) }));
    expect(res.status).toBe(206);
    expect(res.headers.get("content-encoding")).toBeNull();
  });

  it("advertises Accept-Ranges on a full response", () => {
    expect(serveBytes(req(), servable()).headers.get("accept-ranges")).toBe("bytes");
  });
});

describe("parseRange", () => {
  it("parses forms and rejects garbage / multi-range", () => {
    expect(parseRange("bytes=0-9", 100)).toEqual({ start: 0, end: 9 });
    expect(parseRange("bytes=90-", 100)).toEqual({ start: 90, end: 99 });
    expect(parseRange("bytes=-10", 100)).toEqual({ start: 90, end: 99 });
    expect(parseRange("bytes=50-9999", 100)).toEqual({ start: 50, end: 99 }); // clamps end
    expect(parseRange("bytes=200-", 100)).toBe("unsatisfiable");
    expect(parseRange("bytes=0-9,20-29", 100)).toBeNull(); // multi-range → serve full
    expect(parseRange("nonsense", 100)).toBeNull();
  });
});
