"use client";

import { FormEvent, useEffect, useState } from "react";
import { api } from "../../lib/api";
import { Shell } from "../../components/Nav";

type Provider = "local" | "zai" | "nvidia";

type ProviderOption = {
  id: Provider;
  label: string;
  requires_api_key: boolean;
  configured: boolean;
};

type MasterAgentSettings = {
  provider: Provider;
  options: ProviderOption[];
  local_model?: string;
  local_available?: boolean;
};

export default function SettingsPage() {
  const [settings, setSettings] = useState<MasterAgentSettings>();
  const [provider, setProvider] = useState<Provider>("local");
  const [apiKey, setApiKey] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);

  async function load() {
    const current = await api<MasterAgentSettings>(
      "/api/settings/master-agent",
    );
    setSettings(current);
    setProvider(current.provider);
  }

  useEffect(() => {
    void load().catch((e) => setError((e as Error).message));
  }, []);

  async function save(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError("");
    setNotice("");
    const body: { provider: Provider; api_key?: string } = { provider };
    if (provider === "zai" && apiKey.trim()) body.api_key = apiKey.trim();
    try {
      const updated = await api<MasterAgentSettings>(
        "/api/settings/master-agent",
        { method: "POST", body: JSON.stringify(body) },
      );
      setSettings(updated);
      setProvider(updated.provider);
      setApiKey("");
      setNotice("Master agent updated.");
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }

  const selected = settings?.options.find((option) => option.id === provider);

  return (
    <Shell>
      <main className="content">
        <h1>Settings</h1>
        <section className="card">
          <h2>Master agent</h2>
          <p className="muted">
            Choose which model provider handles new Hive requests. API providers
            run without the local model fallback.
          </p>
          {error && (
            <p role="alert" className="error">
              {error}
            </p>
          )}
          {notice && <p role="status">{notice}</p>}
          {!settings ? (
            <p role="status">Loading settings…</p>
          ) : (
            <form onSubmit={save}>
              <label htmlFor="master-agent-provider">Provider</label>
              <select
                id="master-agent-provider"
                aria-label="Master agent provider"
                value={provider}
                onChange={(event) => {
                  setProvider(event.target.value as Provider);
                  setNotice("");
                }}
                disabled={busy}
              >
                {settings.options.map((option) => (
                  <option key={option.id} value={option.id}>
                    {option.label}
                    {option.configured ? " · configured" : " · not configured"}
                  </option>
                ))}
              </select>
              {provider === "local" && settings.local_model && (
                <p className="muted">
                  Model: {settings.local_model}
                  {settings.local_available === false ? " · unavailable" : ""}
                </p>
              )}
              {provider === "zai" && selected?.requires_api_key && (
                <label htmlFor="zai-api-key">
                  Z.AI API key
                  <input
                    id="zai-api-key"
                    type="password"
                    autoComplete="new-password"
                    value={apiKey}
                    onChange={(event) => setApiKey(event.target.value)}
                    placeholder={selected.configured ? "Replace key (optional)" : "Required to enable Z.AI"}
                    disabled={busy}
                  />
                  <span className="muted">
                    The key is sent over this authenticated connection and is
                    never displayed after saving.
                  </span>
                </label>
              )}
              {provider === "nvidia" && (
                <p className="muted">
                  NVIDIA uses the server&apos;s configured NVIDIA_API_KEY_FLASH.
                </p>
              )}
              <button type="submit" disabled={busy}>
                {busy ? "Saving…" : "Save master agent"}
              </button>
            </form>
          )}
        </section>
      </main>
    </Shell>
  );
}
