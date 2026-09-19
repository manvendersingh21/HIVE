"use client";
import { FormEvent, useState } from "react";
export default function LoginPage() {
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (busy) return;
    const body = new URLSearchParams({
      password: String(new FormData(event.currentTarget).get("password") || ""),
    });
    setBusy(true);
    setError("");
    try {
      const response = await fetch("/login", { method: "POST", body });
      if (!response.ok)
        throw new Error(
          response.status === 401
            ? "Incorrect password."
            : "Sign in failed. Please retry.",
        );
      window.location.assign("/");
    } catch (e) {
      setError((e as Error).message);
      setBusy(false);
    }
  }
  return (
    <main
      style={{
        minHeight: "100dvh",
        display: "grid",
        placeItems: "center",
        padding: 24,
      }}
    >
      <form
        className="card"
        style={{ width: "100%", maxWidth: 340, padding: 28 }}
        onSubmit={submit}
      >
        <h1>🐝 Hive</h1>
        <p className="muted">Terminal access</p>
        <label htmlFor="password">Password</label>
        <input
          id="password"
          name="password"
          type="password"
          autoFocus
          autoComplete="current-password"
          required
          style={{ width: "100%" }}
        />
        <button
          className="primary"
          disabled={busy}
          style={{ width: "100%", marginTop: 12 }}
        >
          {busy ? "Signing in…" : "Sign in"}
        </button>
        {error && (
          <p role="alert" className="error">
            {error}
          </p>
        )}
      </form>
    </main>
  );
}
