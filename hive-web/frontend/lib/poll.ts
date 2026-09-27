import { DependencyList, useEffect, useRef } from "react";

/// Resolves once the tab is visible. A hidden tab has nobody to show fresh
/// data to, so polls wait here instead of hitting the server.
export function visible(): Promise<void> {
  if (typeof document === "undefined" || !document.hidden) return Promise.resolve();
  return new Promise((resolve) => {
    const shown = () => {
      if (document.hidden) return;
      document.removeEventListener("visibilitychange", shown);
      resolve();
    };
    document.addEventListener("visibilitychange", shown);
  });
}

/// Run `tick` now, then `ms` after each tick finishes, only while the tab is
/// visible. Ticks never overlap, and returning to the tab refreshes at once.
export function usePoll(tick: () => Promise<void> | void, ms: number, deps: DependencyList) {
  const latest = useRef(tick);
  latest.current = tick;
  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let running = false;
    async function run() {
      clearTimeout(timer);
      if (cancelled || running) return;
      running = true;
      try {
        await visible();
        if (!cancelled) await latest.current();
      } catch {
        /* callers report their own errors */
      } finally {
        running = false;
        if (!cancelled) timer = setTimeout(() => void run(), ms);
      }
    }
    const shown = () => {
      if (!document.hidden) void run();
    };
    document.addEventListener("visibilitychange", shown);
    void run();
    return () => {
      cancelled = true;
      clearTimeout(timer);
      document.removeEventListener("visibilitychange", shown);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ms, ...deps]);
}
