const ROUTES = ["/maker/", "/relayer/", "/issuer/"];
// What the services read, and the body's length. Everything else stays at the edge, the client's
// address that Cloudflare attaches among it.
const FORWARDED_HEADERS = new Set(["authorization", "content-length", "content-type"]);

export async function proxy(request: Request, origin: Pick<Fetcher, "fetch">): Promise<Response> {
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

function failure(status: number, code: "notFound" | "unavailable", error: string): Response {
  return Response.json({ code, error }, { status, headers: { "Cache-Control": "no-store" } });
}

export default {
  fetch(request, env): Promise<Response> {
    return proxy(request, env.ORIGIN);
  },
} satisfies ExportedHandler<Env>;
