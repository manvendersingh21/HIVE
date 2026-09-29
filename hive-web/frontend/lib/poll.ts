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
/// While `enabled` is false it ticks once for the current `deps` and then
/// stops; turning it back on resumes polling straight away.
export function usePoll(
  tick: () => Promise<void> | void,
  ms: number,
  deps: DependencyList,
  enabled = true,
) {
  const latest = useRef(tick);
  latest.current = tick;
  const ticked = useRef(false);
  useEffect(() => {
    ticked.current = false;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  useEffect(() => {
    if (!enabled && ticked.current) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let running = false;
    async function run() {
      clearTimeout(timer);
      if (cancelled || running) return;
      running = true;
      try {
        // Awaiting only when hidden keeps a visible tick synchronous, so a
        // superseded effect can't run the next render's tick before cleanup.
        if (document.hidden) await visible();
        if (!cancelled) {
          // Set first: the tick's own state updates may disable polling.
          ticked.current = true;
          await latest.current();
        }
      } catch {
        /* callers report their own errors */
      } finally {
        running = false;
        if (!cancelled && enabled) timer = setTimeout(() => void run(), ms);
      }
    }
    const shown = () => {
      if (!document.hidden) void run();
    };
    if (enabled) document.addEventListener("visibilitychange", shown);
    void run();
    return () => {
      cancelled = true;
      clearTimeout(timer);
      document.removeEventListener("visibilitychange", shown);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ms, enabled, ...deps]);
}
