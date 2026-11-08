// Who may capture by email, and the request that hands a message to MIS. The message is
// streamed unparsed (MIS parses it), so a Worker stays far under 10 ms CPU.

export interface Env {
  /** Comma-separated envelope senders (SMTP MAIL FROM) that may capture. */
  ALLOWED_FROM: string;
  /** e.g. https://capture.m6o.app/api/capture */
  MIS_CAPTURE_URL: string;
  CAPTURE_TOKEN: string;
  CF_ACCESS_CLIENT_ID: string;
  CF_ACCESS_CLIENT_SECRET: string;
}

/** Why `from` may not capture, or null if it may: listed (ignoring case), and Email
 * Routing's own `Authentication-Results` (`auth`, the first such header) shows SPF passed
 * for it, so the envelope sender isn't forged. */
export function refusal(from: string, auth: string | null, allowed: string): string | null {
  const sender = from.trim().toLowerCase();
  const listed = allowed.split(",").some((a) => a.trim().toLowerCase() === sender);
  if (!sender || !listed) return "sender not allowed";
  // `Headers.get` joins repeats with ", "; Cloudflare's is first. A comma before `spf=`
  // fails closed.
  if (!/^mx\.cloudflare\.net;[^,]*\bspf=pass\b/i.test(auth ?? "")) return "sender not verified (SPF)";
  return null;
}

/** The capture POST: the raw message, the capture token and the Access service token. */
export function captureRequest(env: Env, raw: ReadableStream | Uint8Array): [string, RequestInit] {
  const headers = {
    "Content-Type": "message/rfc822",
    Authorization: `Bearer ${env.CAPTURE_TOKEN}`,
    "CF-Access-Client-Id": env.CF_ACCESS_CLIENT_ID,
    "CF-Access-Client-Secret": env.CF_ACCESS_CLIENT_SECRET,
  };
  return [env.MIS_CAPTURE_URL, { method: "POST", headers, body: raw }];
}

/** The bounce text for MIS's refusal: `413: images are limited to 10 MB`. */
export function bounce(status: number, body: string): string {
  return `${status}: ${body.slice(0, 200)}`;
}

/** Consumes `message.raw` once, or rejects it so the sender gets a bounce saying why. */
export async function handle(message: ForwardableEmailMessage, env: Env, fetcher = fetch): Promise<void> {
  const why = refusal(message.from, message.headers.get("Authentication-Results"), env.ALLOWED_FROM);
  if (why) return message.setReject(why);
  try {
    const res = await fetcher(...captureRequest(env, message.raw));
    if (!res.ok) message.setReject(bounce(res.status, await res.text()));
  } catch (e) {
    message.setReject(`MIS unreachable: ${String(e).slice(0, 200)}`);
  }
}
