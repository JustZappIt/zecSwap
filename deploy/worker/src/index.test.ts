import assert from "node:assert/strict";
import test from "node:test";
import { proxy } from "./index.ts";

const SECRET = "11".repeat(32);
const SWAP = `0x${"ab".repeat(32)}`;

/** The ticket a maker with `secret` gives `swap`. */
async function ticket(swap: string, secret = SECRET): Promise<string> {
  const key = await crypto.subtle.importKey(
    "raw",
    Buffer.from(secret, "hex"),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const mac = await crypto.subtle.sign("HMAC", key, Buffer.from(swap.slice(2), "hex"));
  return `${swap}.${Buffer.from(mac).toString("hex")}`;
}

/** The headers a claim posted with `headers` reaches the origin with. */
async function reaching(headers: Record<string, string>, secret: string | undefined) {
  let seen: Headers | undefined;
  const request = new Request("https://example.workers.dev/relayer/v1/claim", {
    method: "POST",
    headers,
    body: "{}",
  });
  await proxy(
    request,
    {
      async fetch(input) {
        assert.ok(input instanceof Request);
        seen = input.headers;
        return new Response(null, { status: 204 });
      },
    },
    secret,
  );
  assert.ok(seen);
  return seen;
}

test("forwards signed payloads without changing bytes, paths or status", async () => {
  const body = '{"amount":"9007199254740993","signature":"0x0102"}';
  const request = new Request("https://example.workers.dev/relayer/v1/reverse/ready", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body,
  });
  let calls = 0;
  const response = await proxy(request, {
    async fetch(input) {
      assert.ok(input instanceof Request);
      assert.equal(input.url, "http://localhost:8080/relayer/v1/reverse/ready");
      assert.equal(input.method, "POST");
      assert.equal(await input.text(), body);
      calls++;
      return Response.json({ code: "rejected", error: "expired" }, { status: 400 });
    },
  }, SECRET);
  assert.equal(calls, 1);
  assert.equal(response.status, 400);
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.deepEqual(await response.json(), { code: "rejected", error: "expired" });
});

test("only the headers the services read reach the origin", async () => {
  const request = new Request("https://example.workers.dev/maker/v1/quote/0x01/accept", {
    method: "POST",
    headers: {
      authorization: "PrivateToken token=abc",
      "content-type": "application/json",
      "cf-connecting-ip": "203.0.113.7",
      "x-forwarded-for": "203.0.113.7",
      "x-real-ip": "203.0.113.7",
      "cf-ipcountry": "NZ",
      "user-agent": "okhttp/4.12.0",
    },
    body: "{}",
  });
  const response = await proxy(request, {
    async fetch(input) {
      assert.ok(input instanceof Request);
      assert.deepEqual(
        [...input.headers],
        [
          ["authorization", "PrivateToken token=abc"],
          ["content-type", "application/json"],
        ],
      );
      assert.equal(await input.text(), "{}");
      return new Response(null, { status: 204 });
    },
  }, SECRET);
  assert.equal(response.status, 204);
});

test("an origin failure is reported without retrying a financial action", async () => {
  let calls = 0;
  const response = await proxy(new Request("https://example.workers.dev/maker/v1/quote"), {
    async fetch() {
      calls++;
      throw new Error("private upstream details");
    },
  }, SECRET);
  assert.equal(calls, 1);
  assert.equal(response.status, 503);
  assert.deepEqual(await response.json(), {
    code: "unavailable",
    error: "swap service is temporarily unavailable",
  });
});

test("the token issuer's routes reach the origin unchanged", async () => {
  const response = await proxy(new Request("https://example.workers.dev/issuer/v1/token-key"), {
    async fetch(input) {
      assert.ok(input instanceof Request);
      assert.equal(input.url, "http://localhost:8080/issuer/v1/token-key");
      return Response.json({ issuer: "zecswap-testnet-issuer" });
    },
  }, SECRET);
  assert.equal(response.status, 200);
});

test("unknown routes never reach the origin", async () => {
  const response = await proxy(new Request("https://example.workers.dev/admin"), {
    async fetch() {
      assert.fail("unexpected origin request");
    },
  }, SECRET);
  assert.equal(response.status, 404);
});

test("a valid ticket names its swap to the origin, and stays at the edge itself", async () => {
  const headers = await reaching({ "zecswap-ticket": await ticket(SWAP) }, SECRET);
  assert.equal(headers.get("zecswap-swap"), SWAP);
  assert.equal(headers.get("zecswap-ticket"), null);
});

test("a forged ticket names no swap", async () => {
  const other = `0x${"cd".repeat(32)}`;
  const forged = [
    await ticket(SWAP, "22".repeat(32)),
    (await ticket(SWAP)).replace(SWAP, other),
    `${SWAP}.${"00".repeat(32)}`,
    (await ticket(SWAP)).toUpperCase(),
    SWAP,
  ];
  for (const ticket of forged) {
    assert.equal((await reaching({ "zecswap-ticket": ticket }, SECRET)).get("zecswap-swap"), null, ticket);
  }
});

test("a request without a ticket names no swap", async () => {
  const headers = await reaching({ "content-type": "application/json" }, SECRET);
  assert.equal(headers.get("zecswap-swap"), null);
});

test("a swap header from the client never reaches the origin", async () => {
  const other = `0x${"cd".repeat(32)}`;
  assert.equal((await reaching({ "zecswap-swap": other }, SECRET)).get("zecswap-swap"), null);
  const ticketed = await reaching(
    { "zecswap-swap": other, "zecswap-ticket": await ticket(SWAP) },
    SECRET,
  );
  assert.equal(ticketed.get("zecswap-swap"), SWAP);
});

test("without a usable secret no ticket names a swap", async () => {
  for (const secret of [undefined, "", "11".repeat(31)]) {
    const headers = await reaching({ "zecswap-ticket": await ticket(SWAP) }, secret);
    assert.equal(headers.get("zecswap-swap"), null);
  }
});
