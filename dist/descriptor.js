export function buildDescriptor(origin, dataset) {
    const { meta, labels, vectors } = dataset;
    // Version-stamp the blob URLs (`?v=<datasetVersion>`) so a CDN/proxy can cache them immutably: the same
    // path with a new version is a new URL, so a republish never serves stale bytes. The Den app ignores the
    // query (it keys its on-device cache by sha256), and `isSyncableUrl` still accepts it.
    const v = encodeURIComponent(meta.datasetVersion);
    return {
        datasetVersion: meta.datasetVersion,
        taxonomyVersion: meta.taxonomyVersion,
        embeddingModel: meta.embeddingModel,
        dims: meta.dims,
        count: meta.count,
        quantization: meta.quantization,
        labels: { url: `${origin}/${labels.name}?v=${v}`, sha256: labels.sha256, bytes: labels.size },
        vectors: { url: `${origin}/${vectors.name}?v=${v}`, sha256: vectors.sha256, bytes: vectors.size },
    };
}
/**
 * The on-device acceptance guards, replicated so a bad build fails Atlas's own tests instead of silently
 * shipping a descriptor Den will reject. Kept in lockstep with `DatasetDescriptor.indexManifest()`.
 */
const SAFE_TOKEN = /^[A-Za-z0-9._-]{1,64}$/;
const HEX = /^[0-9a-fA-F]{16,128}$/;
export function isAppAcceptable(d) {
    const blobOk = (b) => HEX.test(b.sha256) && b.bytes > 0 && isSyncableUrl(b.url);
    return (d.dims > 0 &&
        d.count > 0 &&
        SAFE_TOKEN.test(d.embeddingModel) &&
        d.embeddingModel !== "." &&
        d.embeddingModel !== ".." &&
        SAFE_TOKEN.test(d.taxonomyVersion) &&
        blobOk(d.labels) &&
        blobOk(d.vectors));
}
/** https anywhere, or http only on localhost / a private-range (LAN) host — the app's SSRF rule. */
export function isSyncableUrl(raw) {
    let url;
    try {
        url = new URL(raw);
    }
    catch {
        return false;
    }
    if (url.protocol === "https:")
        return true;
    if (url.protocol !== "http:")
        return false;
    return isLocalHost(url.hostname);
}
function isLocalHost(host) {
    if (host === "localhost" || host === "127.0.0.1" || host === "::1" || host.endsWith(".local"))
        return true;
    const octets = host.split(".").map((o) => Number(o));
    if (octets.length !== 4 || octets.some((o) => !Number.isInteger(o)))
        return false;
    if (octets[0] === 10)
        return true; // 10.0.0.0/8
    if (octets[0] === 192 && octets[1] === 168)
        return true; // 192.168.0.0/16
    if (octets[0] === 172 && octets[1] >= 16 && octets[1] <= 31)
        return true; // 172.16.0.0/12
    return false;
}
