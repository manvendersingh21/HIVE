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

type FleetWorker = {
  name: string;
  host: string;
  user: string;
  port: number | null;
  tags: string[];
  status: "online" | "busy" | "offline" | "unhealthy";
  removable: boolean;
};

export default function SettingsPage() {
  const [settings, setSettings] = useState<MasterAgentSettings>();
  const [provider, setProvider] = useState<Provider>("local");
  const [apiKey, setApiKey] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);

  const [fleet, setFleet] = useState<FleetWorker[]>();
  const [fleetError, setFleetError] = useState("");
  const [fleetBusy, setFleetBusy] = useState(false);
  const [name, setName] = useState("");
  const [host, setHost] = useState("");
  const [user, setUser] = useState("");
  const [tags, setTags] = useState("");

  async function load() {
    const current = await api<MasterAgentSettings>(
      "/api/settings/master-agent",
    );
    setSettings(current);
    setProvider(current.provider);
  }

  async function loadFleet() {
    setFleet(await api<FleetWorker[]>("/api/fleet"));
  }

  useEffect(() => {
    void load().catch((e) => setError((e as Error).message));
    void loadFleet().catch((e) => {
      setFleetError((e as Error).message);
      setFleet([]);
    });
  }, []);

  async function addWorker(event: FormEvent) {
    event.preventDefault();
    setFleetBusy(true);
    setFleetError("");
    try {
      const updated = await api<FleetWorker[]>("/api/fleet", {
        method: "POST",
        body: JSON.stringify({
          name: name.trim(),
          host: host.trim(),
          user: user.trim(),
          tags: tags
            .split(",")
            .map((t) => t.trim())
            .filter(Boolean),
        }),
      });
      setFleet(updated);
      setName("");
      setHost("");
      setUser("");
      setTags("");
    } catch (e) {
      setFleetError((e as Error).message);
    } finally {
      setFleetBusy(false);
    }
  }

  async function removeWorker(workerName: string) {
    setFleetBusy(true);
    setFleetError("");
    try {
      setFleet(
        await api<FleetWorker[]>(
          `/api/fleet/${encodeURIComponent(workerName)}`,
          { method: "DELETE" },
        ),
      );
    } catch (e) {
      setFleetError((e as Error).message);
    } finally {
      setFleetBusy(false);
    }
  }

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
        <section className="card">
          <h2>Fleet</h2>
          <p className="muted">
            SSH worker machines Hive can delegate to. Machines added here are
            stored separately from <code>config/workers.toml</code> and can be
            removed the same way; machines from that file can only be edited
            there.
          </p>
          {fleetError && (
            <p role="alert" className="error">
              {fleetError}
            </p>
          )}
          {!fleet ? (
            <p role="status">Loading fleet…</p>
          ) : (
            <>
              {!fleet.length ? (
                <div className="empty">No worker machines configured.</div>
              ) : (
                fleet.map((worker) => (
                  <article className="card" key={worker.name}>
                    <div className="row">
                      <span
                        className={`dot ${worker.status === "online" ? "on" : ""}`}
                      />
                      <strong>{worker.name}</strong>
                      <span className="muted">
                        {worker.user}@{worker.host}
                        {worker.port ? `:${worker.port}` : ""}
                      </span>
                      <span>{worker.status}</span>
                      {worker.removable && (
                        <button
                          type="button"
                          disabled={fleetBusy}
                          onClick={() => void removeWorker(worker.name)}
                        >
                          Remove
                        </button>
                      )}
                    </div>
                    {worker.tags.length > 0 && (
                      <div className="row">
                        {worker.tags.map((tag) => (
                          <span className="pill" key={tag}>
                            {tag}
                          </span>
                        ))}
                      </div>
                    )}
                  </article>
                ))
              )}
              <form onSubmit={addWorker}>
                <label htmlFor="worker-name">
                  Name
                  <input
                    id="worker-name"
                    value={name}
                    onChange={(event) => setName(event.target.value)}
                    placeholder="e.g. gpu-node"
                    disabled={fleetBusy}
                    required
                  />
                </label>
                <label htmlFor="worker-host">
                  Host
                  <input
                    id="worker-host"
                    value={host}
                    onChange={(event) => setHost(event.target.value)}
                    placeholder="~/.ssh/config alias, hostname, or IP"
                    disabled={fleetBusy}
                    required
                  />
                </label>
                <label htmlFor="worker-user">
                  SSH user
                  <input
                    id="worker-user"
                    value={user}
                    onChange={(event) => setUser(event.target.value)}
                    disabled={fleetBusy}
                    required
                  />
                </label>
                <label htmlFor="worker-tags">
                  Tags (comma-separated, optional)
                  <input
                    id="worker-tags"
                    value={tags}
                    onChange={(event) => setTags(event.target.value)}
                    placeholder="gpu, shared, slurm"
                    disabled={fleetBusy}
                  />
                </label>
                <button type="submit" disabled={fleetBusy}>
                  {fleetBusy ? "Adding…" : "Add machine"}
                </button>
              </form>
            </>
          )}
        </section>
      </main>
    </Shell>
  );
}
