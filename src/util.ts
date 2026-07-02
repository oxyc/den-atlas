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
