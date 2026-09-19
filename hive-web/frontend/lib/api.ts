export async function request(
  url: string,
  init?: RequestInit,
): Promise<Response> {
  const headers = new Headers(init?.headers);
  if (typeof init?.body === "string" && !headers.has("content-type"))
    headers.set("content-type", "application/json");
  const response = await fetch(url, { cache: "no-store", ...init, headers });
  if (
    response.redirected &&
    new URL(response.url).pathname.replace(/\/$/, "") === "/login"
  ) {
    window.location.assign("/login/");
    throw new Error("Your session expired. Please sign in again.");
  }
  if (!response.ok) {
    if (response.status === 401) window.location.assign("/login/");
    const body = await response.text();
    let message = body;
    try {
      message = JSON.parse(body).error || body;
    } catch {
      /* plain text errors */
    }
    throw new Error(message || `Request failed (${response.status})`);
  }
  return response;
}

export async function api<T = unknown>(
  url: string,
  init?: RequestInit,
): Promise<T> {
  const response = await request(url, init);
  return response.status === 204
    ? (undefined as T)
    : (response.json() as Promise<T>);
}
export async function apiText(url: string): Promise<string> {
  return (await request(url)).text();
}
export function terminalUrl(name: string, host: string) {
  return `/terminal/?name=${encodeURIComponent(name)}&host=${encodeURIComponent(host)}`;
}

// Tailnet HTTP origins do not expose randomUUID; getRandomValues is available
// there too. Preserve the backend's UUID v4 request identity requirement.
export function requestId(): string {
  if (typeof crypto.randomUUID === "function") return crypto.randomUUID();
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
  return [
    hex.slice(0, 8),
    hex.slice(8, 12),
    hex.slice(12, 16),
    hex.slice(16, 20),
    hex.slice(20),
  ].join("-");
}
