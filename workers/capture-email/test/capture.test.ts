// Run: npm test (node --experimental-strip-types --test test/*.test.ts)
import { test } from "node:test";
import assert from "node:assert/strict";
import { bounce, captureRequest, handle, refusal, type Env } from "../src/capture.ts";

const env: Env = {
  ALLOWED_FROM: "a@example.com, B@Example.org",
  MIS_CAPTURE_URL: "https://capture.example.com/api/capture",
  CAPTURE_TOKEN: "tok",
  CF_ACCESS_CLIENT_ID: "id.access",
  CF_ACCESS_CLIENT_SECRET: "secret",
};
const SPF_PASS = "mx.cloudflare.net; dkim=pass header.d=example.org; spf=pass smtp.mailfrom=b@example.org; dmarc=pass";

test("only listed senders whose SPF passed at Cloudflare may capture", () => {
  for (const from of ["a@example.com", "b@example.org", " B@EXAMPLE.ORG "]) {
    assert.equal(refusal(from, SPF_PASS, env.ALLOWED_FROM), null, from);
  }
  // Cloudflare's documented form, and one with a comment and a HELO result.
  for (const auth of [
    "mx.cloudflare.net; spf=pass; dkim=pass; dmarc=pass",
    "mx.cloudflare.net; spf=pass (domain of a@example.com; designates 192.0.2.1) smtp.mailfrom=a@example.com; spf=pass smtp.helo=mail.example.com",
  ]) {
    assert.equal(refusal("a@example.com", auth, env.ALLOWED_FROM), null, auth);
  }
  for (const from of ["", "c@example.com", "a@example.com.evil", "xa@example.com"]) {
    assert.equal(refusal(from, SPF_PASS, env.ALLOWED_FROM), "sender not allowed", from);
  }
  assert.equal(refusal("a@example.com", SPF_PASS, ""), "sender not allowed");
  for (const auth of [
    null,
    "mx.cloudflare.net; spf=fail smtp.mailfrom=a@example.com; dkim=pass",
    "mx.cloudflare.net; spf=softfail; dmarc=none",
    // A sender-supplied header after Cloudflare's doesn't count.
    "mx.cloudflare.net; spf=none, mx.evil.example; spf=pass",
    "mx.evil.example; spf=pass",
    "mx.cloudflare.net.evil.example; spf=pass",
    // `spf=pass` inside a sender-controlled value.
    "mx.cloudflare.net; dkim=pass header.i=spf=pass@evil.example; spf=fail",
    // HELO passed, MAIL FROM didn't.
    "mx.cloudflare.net; spf=pass smtp.helo=evil.example; spf=fail smtp.mailfrom=a@example.com",
    "mx.cloudflare.net (spf=pass); spf=fail",
  ]) {
    assert.equal(refusal("a@example.com", auth, env.ALLOWED_FROM), "sender not verified (SPF)", String(auth));
  }
});

test("the request carries the raw message and every credential", () => {
  const raw = new Uint8Array([1, 2, 3]);
  const [url, init] = captureRequest(env, raw);
  assert.equal(url, env.MIS_CAPTURE_URL);
  assert.equal(init.method, "POST");
  assert.equal(init.body, raw);
  assert.deepEqual(init.headers, {
    "Content-Type": "message/rfc822",
    Authorization: "Bearer tok",
    "CF-Access-Client-Id": "id.access",
    "CF-Access-Client-Secret": "secret",
  });
});

test("bounces say what MIS said, capped", () => {
  assert.equal(bounce(413, "images are limited to 10 MB"), "413: images are limited to 10 MB");
  assert.equal(bounce(400, "x".repeat(500)).length, "400: ".length + 200);
});

// A stand-in for Email Routing's message: records setReject.
function message(from: string, auth: string | null = SPF_PASS) {
  const m = {
    from,
    raw: new Uint8Array([9]),
    headers: new Headers(auth ? { "Authentication-Results": auth } : {}),
    rejected: null as string | null,
    setReject(why: string) {
      m.rejected = why;
    },
  };
  return m;
}

test("handle posts allowed mail and bounces refusals", async () => {
  const sent: [string, RequestInit][] = [];
  const replying = (status: number, body: string) => async (url: string, init: RequestInit) => {
    sent.push([url, init]);
    return new Response(body, { status });
  };
  const ok = message("a@example.com");
  await handle(ok as never, env, replying(201, "{}") as never);
  assert.equal(ok.rejected, null);
  assert.equal(sent.length, 1);
  assert.equal(sent[0][1].body, ok.raw);

  const refused = message("a@example.com");
  await handle(refused as never, env, replying(400, "send a url, text or image") as never);
  assert.equal(refused.rejected, "400: send a url, text or image");

  const stranger = message("c@example.com");
  await handle(stranger as never, env, replying(201, "{}") as never);
  assert.equal(stranger.rejected, "sender not allowed");
  assert.equal(sent.length, 2, "strangers never reach MIS");

  const down = message("a@example.com");
  await handle(down as never, env, (async () => {
    throw new TypeError("fetch failed");
  }) as never);
  assert.equal(down.rejected, "MIS unreachable: TypeError: fetch failed");
});
