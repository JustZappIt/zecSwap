const ROUTES = ["/maker/", "/relayer/", "/issuer/"];
// What the services read, and the body's length. Everything else stays at the edge, the client's
// address that Cloudflare attaches among it.
const FORWARDED_HEADERS = new Set(["authorization", "content-length", "content-type"]);
// The maker gives each swap a ticket at accept, which its relayer requests carry. Only a ticket
// that checks out names its swap to the origin, which gives each such swap an allowance of its own.
const TICKET_HEADER = "zecswap-ticket";
const SWAP_HEADER = "zecswap-swap";
const TICKET = /^(0x[0-9a-f]{64})\.([0-9a-f]{64})$/;
const SECRET = /^(?:0x)?([0-9a-fA-F]{64})$/;

export async function proxy(
  request: Request,
  origin: Pick<Fetcher, "fetch">,
  ticketSecret: string | undefined,
): Promise<Response> {
  const url = new URL(request.url);
  if (!ROUTES.some((route) => url.pathname.startsWith(route))) {
    return failure(404, "notFound", "route not found");
  }
  url.protocol = "http:";
  url.hostname = "localhost";
  url.port = "8080";
  const forwarded = new Request(url, request);
  for (const name of [...forwarded.headers.keys()]) {
    if (!FORWARDED_HEADERS.has(name)) forwarded.headers.delete(name);
  }
  const swap = await ticketedSwap(request.headers.get(TICKET_HEADER), ticketSecret);
  if (swap) forwarded.headers.set(SWAP_HEADER, swap);

  try {
    const upstream = await origin.fetch(forwarded);
    const response = new Response(upstream.body, upstream);
    response.headers.set("Cache-Control", "no-store");
    return response;
  } catch {
    console.error(JSON.stringify({ event: "origin_unavailable" }));
    return failure(503, "unavailable", "swap service is temporarily unavailable");
  }
}

/** The swap a ticket is for, if it is the HMAC-SHA256 of the swap id under the secret. */
async function ticketedSwap(ticket: string | null, secret: string | undefined): Promise<string | null> {
  const [, swap, mac] = TICKET.exec(ticket ?? "") ?? [];
  const [, key] = SECRET.exec(secret ?? "") ?? [];
  if (!swap || !mac) return null;
  if (!key) {
    console.error(JSON.stringify({ event: "ticket_secret_unusable" }));
    return null;
  }
  const hmac = await crypto.subtle.importKey("raw", bytes(key), { name: "HMAC", hash: "SHA-256" }, false, [
    "verify",
  ]);
  return (await crypto.subtle.verify("HMAC", hmac, bytes(mac), bytes(swap.slice(2)))) ? swap : null;
}

function bytes(hex: string): Uint8Array {
  return Uint8Array.from(hex.match(/../g) ?? [], (pair) => parseInt(pair, 16));
}

function failure(status: number, code: "notFound" | "unavailable", error: string): Response {
  return Response.json({ code, error }, { status, headers: { "Cache-Control": "no-store" } });
}

export default {
  fetch(request, env): Promise<Response> {
    return proxy(request, env.ORIGIN, env.TICKET_SECRET);
  },
} satisfies ExportedHandler<Env>;
