/** Small shared helpers (mirrors the den-scout house style). */

/** JSON `Response` with the right content-type. */
export function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** HTML `Response`. */
export function html(body: string): Response {
  return new Response(body, { headers: { "content-type": "text/html; charset=utf-8" } });
}

/** FNV-1a 32-bit → 8-char hex. A cheap, sync content fingerprint for the small JSON responses' ETags (the
 * big blobs use their real sha256). Runtime-agnostic (no crypto import), so it works on Node and a Worker. */
export function fnv1a(input: string): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < input.length; i++) {
    h ^= input.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return (h >>> 0).toString(16).padStart(8, "0");
}

/**
 * The public origin to build absolute blob URLs from. Behind Caddy the socket is plain http on an
 * internal host, but Caddy forwards `X-Forwarded-Proto` + `Host`, so honor those to emit the correct
 * `https://atlas.<domain>` the Den app can reach (matches the scout / trailer-service convention).
 * `PUBLIC_BASE_URL` overrides everything (e.g. a CDN in front of the blobs).
 */
export function publicOrigin(request: Request, override?: string): string {
  if (override) return override.replace(/\/$/, "");
  const url = new URL(request.url);
  const proto = request.headers.get("x-forwarded-proto")?.split(",")[0]?.trim() || url.protocol.replace(/:$/, "");
  const host =
    request.headers.get("x-forwarded-host")?.split(",")[0]?.trim() || request.headers.get("host") || url.host;
  return `${proto}://${host}`;
}
