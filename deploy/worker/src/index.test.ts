import assert from "node:assert/strict";
import test from "node:test";
import { proxy } from "./index.ts";

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
  });
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
  });
  assert.equal(response.status, 204);
});

test("an origin failure is reported without retrying a financial action", async () => {
  let calls = 0;
  const response = await proxy(new Request("https://example.workers.dev/maker/v1/quote"), {
    async fetch() {
      calls++;
      throw new Error("private upstream details");
    },
  });
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
  });
  assert.equal(response.status, 200);
});

test("unknown routes never reach the origin", async () => {
  const response = await proxy(new Request("https://example.workers.dev/admin"), {
    async fetch() {
      assert.fail("unexpected origin request");
    },
  });
  assert.equal(response.status, 404);
});
