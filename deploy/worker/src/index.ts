export async function proxy(request: Request, origin: Pick<Fetcher, "fetch">): Promise<Response> {
  const url = new URL(request.url);
  if (!url.pathname.startsWith("/maker/") && !url.pathname.startsWith("/relayer/")) {
    return failure(404, "notFound", "route not found");
  }
  url.protocol = "http:";
  url.hostname = "localhost";
  url.port = "8080";

  try {
    const upstream = await origin.fetch(new Request(url, request));
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
