"use client";
import Link from "next/link";
import { usePathname } from "next/navigation";
import { useState } from "react";
import { api } from "../lib/api";
import { usePoll } from "../lib/poll";

const ATTENTION = ["awaiting-approval", "needs-setup"];

export function Nav() {
  const pathname = usePathname().replace(/\/$/, "") || "/";
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [attention, setAttention] = useState(0);
  // Runs parked on a person (an approval or a failed setup) are easy to miss
  // from inside a chat; count them on every page.
  usePoll(
    async () => {
      try {
        // The server filters; the client check keeps older servers correct.
        const runs = await api<{ state: string }[]>(`/api/runs?state=${ATTENTION.join(",")}`);
        setAttention(runs.filter((r) => ATTENTION.includes(r.state)).length);
      } catch {
        /* delegation disabled or offline: no badge */
      }
    },
    10000,
    [],
  );
  const links = [
    ["/", "Agent"],
    ["/sessions/", "Sessions"],
    ["/machines/", "Machines"],
    ["/incidents/", "Incidents"],
    ["/settings/", "Settings"],
  ];
  async function signOut() {
    setBusy(true);
    setError("");
    try {
      const response = await fetch("/logout", { method: "POST" });
      if (!response.ok) throw new Error("Sign out failed. Please retry.");
      window.location.assign("/login/");
    } catch (e) {
      setError((e as Error).message);
      setBusy(false);
    }
  }
  return (
    <>
      <header className="topbar">
        <Link className="brand" href="/">
          🐝 Hive
        </Link>
        <nav aria-label="Main navigation" style={{ gap: "4px" }}>
          {links.map(([href, label]) => (
            <Link
              key={href}
              aria-current={
                pathname === (href.replace(/\/$/, "") || "/")
                  ? "page"
                  : undefined
              }
              href={href}
            >
              {label}
              {href === "/sessions/" && attention > 0 && (
                <span className="nav-count" title={`${attention} session${attention === 1 ? "" : "s"} need you`}>
                  {attention}
                </span>
              )}
            </Link>
          ))}
        </nav>
        <button className="ghost" disabled={busy} onClick={signOut}>
          {busy ? "Signing out…" : "Sign out"}
        </button>
      </header>
      {error && (
        <p role="alert" className="error">
          {error}
        </p>
      )}
    </>
  );
}
export function Shell({ children }: { children: React.ReactNode }) {
  return (
    <div className="shell">
      <Nav />
      {children}
    </div>
  );
}
