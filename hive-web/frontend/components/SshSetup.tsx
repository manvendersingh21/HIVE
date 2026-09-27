"use client";
import { useRef, useState } from "react";
import { api } from "../lib/api";

export type KeyInfo = {
  public_key: string | null;
  path: string;
  config_included: boolean;
};
export type Target = { host: string; user: string; port: number | null };
type Status =
  | "ok"
  | "host-key-unknown"
  | "host-key-changed"
  | "not-authorized"
  | "unresolvable"
  | "unreachable"
  | "error";
type TestResult = {
  status: Status;
  message: string;
  methods: string[];
  password_possible: boolean;
};
type Fingerprint = { fingerprint: string; kind: string };

export const authorizeCommand = (key: string) =>
  `mkdir -p ~/.ssh && chmod 700 ~/.ssh && echo '${key}' >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys`;

/// Text a person copies to another machine. The page is often served over
/// plain http on a tailnet address, where the Clipboard API doesn't exist, so
/// fall back to selecting the text.
function CopyField({ label, text }: { label: string; text: string }) {
  const box = useRef<HTMLTextAreaElement>(null);
  const [copied, setCopied] = useState("");
  async function copy() {
    try {
      if (navigator.clipboard) await navigator.clipboard.writeText(text);
      else {
        box.current?.select();
        if (!document.execCommand("copy")) throw new Error();
      }
      setCopied("Copied.");
    } catch {
      box.current?.select();
      setCopied("Selected. Press Ctrl+C or ⌘C to copy.");
    }
  }
  return (
    <div className="copy-field">
      <textarea
        ref={box}
        aria-label={label}
        className="mono small"
        readOnly
        rows={3}
        value={text}
        onFocus={(e) => e.currentTarget.select()}
      />
      <div className="row">
        <button type="button" onClick={() => void copy()}>
          Copy
        </button>
        {copied && (
          <span role="status" className="muted small">
            {copied}
          </span>
        )}
      </div>
    </div>
  );
}

export function HiveKey({
  keyInfo,
  onChange,
}: {
  keyInfo?: KeyInfo;
  onChange: (key: KeyInfo) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  async function create() {
    setBusy(true);
    setError("");
    try {
      onChange(await api<KeyInfo>("/api/fleet/ssh/key", { method: "POST", body: "{}" }));
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  return (
    <section className="card ssh-key">
      <h3>Hive&apos;s SSH key</h3>
      {!keyInfo ? (
        <p role="status" className="muted">Loading key…</p>
      ) : keyInfo.public_key ? (
        <>
          <p className="muted small">
            Machines must authorize this key so Hive can sign in without a
            password. It lives at <code>{keyInfo.path}</code>.
          </p>
          <CopyField label="Hive's public key" text={keyInfo.public_key} />
        </>
      ) : (
        <>
          <p className="muted small">
            Hive has no SSH key yet. Create one, then authorize it on each
            machine.
          </p>
          <button type="button" className="primary" disabled={busy} onClick={() => void create()}>
            {busy ? "Creating…" : "Create Hive's key"}
          </button>
        </>
      )}
      {keyInfo && !keyInfo.config_included && (
        <p role="alert" className="banner bad">
          Your <code>~/.ssh/config</code> doesn&apos;t include{" "}
          <code>~/.ssh/config.d/*.conf</code>, so machines added here won&apos;t
          use Hive&apos;s key. Add <code>Include ~/.ssh/config.d/*.conf</code>{" "}
          as its first line.
        </p>
      )}
      {error && (
        <p role="alert" className="error">
          {error}
        </p>
      )}
    </section>
  );
}

const post = <T,>(path: string, body: unknown) =>
  api<T>(path, { method: "POST", body: JSON.stringify(body) });

/// Test a machine before adding it, and walk through whatever stands in the
/// way: an unknown host key, Hive's key not being authorized yet.
export function ConnectionCheck({ target, keyInfo }: { target: Target; keyInfo?: KeyInfo }) {
  const [result, setResult] = useState<TestResult>();
  const [checked, setChecked] = useState("");
  const [fingerprints, setFingerprints] = useState<Fingerprint[]>();
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const ready = !!target.host.trim() && !!target.user.trim();
  const who = `${target.user.trim()}@${target.host.trim()}`;
  // A result describes the machine it was checked against; editing the form
  // clears it rather than leaving a stale answer.
  const current = JSON.stringify(target);
  if (checked && checked !== current) {
    setChecked("");
    setResult(undefined);
    setFingerprints(undefined);
    setPassword("");
    setError("");
  }
  async function step<T>(label: string, action: () => Promise<T>, then: (value: T) => void) {
    setBusy(label);
    setError("");
    try {
      then(await action());
      setChecked(current);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy("");
    }
  }
  const test = () =>
    step("test", () => post<TestResult>("/api/fleet/ssh/test", target), (r) => {
      setResult(r);
      setFingerprints(undefined);
    });
  const scan = () =>
    step("scan", () => post<{ fingerprints: Fingerprint[] }>("/api/fleet/ssh/host-key", target), (r) =>
      setFingerprints(r.fingerprints),
    );
  const trust = () =>
    step("trust", () => post<TestResult>("/api/fleet/ssh/trust", { ...target, fingerprints }), (r) => {
      setResult(r);
      setFingerprints(undefined);
    });
  async function install() {
    const secret = password;
    // Never keep the password around, whatever the outcome.
    setPassword("");
    await step("install", () => post<TestResult>("/api/fleet/ssh/install-key", { ...target, password: secret }), setResult);
  }
  return (
    <div className="ssh-check">
      <button type="button" disabled={!ready || !!busy} onClick={() => void test()}>
        {busy === "test" ? "Testing…" : "Test connection"}
      </button>
      {result && (
        <div className={`banner ${result.status === "ok" ? "good" : result.status === "host-key-changed" ? "bad" : ""}`} role="status" data-ssh-status={result.status}>
          <strong>
            {result.status === "ok"
              ? "Connected"
              : result.status === "host-key-unknown"
                ? "New machine"
                : result.status === "host-key-changed"
                  ? "Host key changed"
                  : result.status === "not-authorized"
                    ? "Hive's key isn't authorized"
                    : "Can't connect"}
          </strong>
          <div>{result.message}</div>
          {result.status === "host-key-unknown" &&
            (!fingerprints ? (
              <div className="row">
                <button type="button" disabled={!!busy} onClick={() => void scan()}>
                  {busy === "scan" ? "Checking…" : "Show host key"}
                </button>
              </div>
            ) : (
              <div className="ssh-step">
                <p className="small">
                  {target.host} presents this key. On that machine, run{" "}
                  <code>ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub</code>{" "}
                  and trust it only if the fingerprint is identical.
                </p>
                <ul className="mono small">
                  {fingerprints.map((f) => (
                    <li key={f.fingerprint}>
                      {f.kind} {f.fingerprint}
                    </li>
                  ))}
                </ul>
                <div className="row">
                  <button type="button" className="primary" disabled={!!busy} onClick={() => void trust()}>
                    {busy === "trust" ? "Trusting…" : "Trust this key"}
                  </button>
                </div>
              </div>
            ))}
          {result.status === "not-authorized" && (
            <div className="ssh-step">
              {keyInfo?.public_key ? (
                <>
                  <p className="small">Run this on {target.host} as {target.user.trim()}:</p>
                  <CopyField label="Command that authorizes Hive's key" text={authorizeCommand(keyInfo.public_key)} />
                </>
              ) : (
                <p className="small">Create Hive&apos;s key above first.</p>
              )}
              {result.password_possible ? (
                // Not a <form>: this sits inside the add-machine form, and a
                // nested form would let Enter submit that one instead.
                <div className="row" role="group" aria-label="Install Hive's key with a password">
                  <label className="grow">
                    Or sign in once with {who}&apos;s password
                    <input
                      type="password"
                      autoComplete="off"
                      aria-label={`Password for ${who}`}
                      value={password}
                      onChange={(e) => setPassword(e.target.value)}
                      onKeyDown={(e) => {
                        if (e.key !== "Enter") return;
                        e.preventDefault();
                        if (password && !busy && keyInfo?.public_key) void install();
                      }}
                      disabled={!!busy || !keyInfo?.public_key}
                    />
                  </label>
                  <button type="button" className="primary" disabled={!!busy || !password || !keyInfo?.public_key} onClick={() => void install()}>
                    {busy === "install" ? "Installing…" : "Install Hive's key"}
                  </button>
                  <p className="muted small full">
                    Hive uses the password once to add its key and never stores it.
                  </p>
                </div>
              ) : (
                <p className="muted small">
                  {target.host} doesn&apos;t accept password sign-in (it offered{" "}
                  {result.methods.join(", ") || "no methods"}), for example because
                  it uses two-factor sign-in. Use the command above instead.
                </p>
              )}
            </div>
          )}
        </div>
      )}
      {error && (
        <p role="alert" className="error">
          {error}
        </p>
      )}
    </div>
  );
}
