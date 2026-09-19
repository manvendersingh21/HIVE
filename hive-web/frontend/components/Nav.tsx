"use client";
import Link from "next/link";
import { usePathname } from "next/navigation";
import { useState } from "react";

export function Nav() {
  const pathname = usePathname().replace(/\/$/, "") || "/";
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
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
