export function serveBytes(request, s) {
    const etag = `"${s.etag}"`;
    const headers = {
        etag,
        "cache-control": s.cacheControl,
        "accept-ranges": "bytes",
        vary: "Accept-Encoding",
    };
    if (s.lastModified)
        headers["last-modified"] = s.lastModified;
    if (isNotModified(request, etag, s.lastModified)) {
        return new Response(null, { status: 304, headers });
    }
    const isHead = request.method.toUpperCase() === "HEAD";
    const rangeHeader = request.headers.get("range");
    // Range wins over gzip: byte ranges are defined on the identity representation.
    const useGzip = !rangeHeader && s.gzip !== undefined && acceptsGzip(request);
    if (rangeHeader) {
        const range = parseRange(rangeHeader, s.bytes.length);
        if (range === "unsatisfiable") {
            return new Response(null, {
                status: 416,
                headers: { ...headers, "content-range": `bytes */${s.bytes.length}`, "content-type": s.contentType },
            });
        }
        if (range) {
            const slice = s.bytes.subarray(range.start, range.end + 1);
            return new Response(isHead ? null : slice, {
                status: 206,
                headers: {
                    ...headers,
                    "content-type": s.contentType,
                    "content-range": `bytes ${range.start}-${range.end}/${s.bytes.length}`,
                    "content-length": String(slice.length),
                },
            });
        }
        // Malformed / multi-range → fall through to a full 200.
    }
    const body = useGzip ? s.gzip : s.bytes;
    const full = {
        ...headers,
        "content-type": s.contentType,
        "content-length": String(body.length),
    };
    if (useGzip)
        full["content-encoding"] = "gzip";
    return new Response(isHead ? null : body, { status: 200, headers: full });
}
/** True if the request's validators say the client's copy is current (→ 304). `If-None-Match` takes
 * precedence over `If-Modified-Since` (RFC 9110 §13.1.3). */
export function isNotModified(request, etagQuoted, lastModified) {
    const inm = request.headers.get("if-none-match");
    if (inm !== null) {
        const tokens = inm.split(",").map((t) => t.trim().replace(/^W\//, ""));
        return tokens.includes("*") || tokens.includes(etagQuoted);
    }
    const ims = request.headers.get("if-modified-since");
    if (ims && lastModified) {
        const since = Date.parse(ims);
        const modified = Date.parse(lastModified);
        if (!Number.isNaN(since) && !Number.isNaN(modified))
            return modified <= since;
    }
    return false;
}
function acceptsGzip(request) {
    const ae = request.headers.get("accept-encoding") ?? "";
    return ae.split(",").some((part) => {
        const [enc, ...params] = part.trim().split(";");
        if (enc !== "gzip")
            return false;
        const q = params.map((p) => p.trim()).find((p) => p.startsWith("q="));
        return !q || Number(q.slice(2)) > 0; // honor `gzip;q=0` (explicit refusal)
    });
}
/** Parse a single-range `Range: bytes=…`. Returns an inclusive {start,end}, `"unsatisfiable"` (→ 416), or
 * `null` (no range / multi-range / malformed → serve the full 200). */
export function parseRange(header, size) {
    const m = /^bytes=(\d*)-(\d*)$/.exec(header.trim());
    if (!m)
        return null; // multi-range or garbage — ignore, serve full
    const [, a, b] = m;
    if (a === "" && b === "")
        return null;
    let start;
    let end;
    if (a === "") {
        const suffix = Number(b);
        if (suffix <= 0)
            return "unsatisfiable";
        start = Math.max(0, size - suffix);
        end = size - 1;
    }
    else {
        start = Number(a);
        if (start >= size)
            return "unsatisfiable";
        end = b === "" ? size - 1 : Math.min(Number(b), size - 1);
        if (start > end)
            return "unsatisfiable";
    }
    return { start, end };
}
