/**
 * Deferred deletes, so a delete can be undone. The mail leaves the list at
 * once, but the request that removes it for good waits out the undo window.
 * Undo cancels it; closing the page sends every waiting delete straight away
 * (with `keepalive`, so it outlives the page) rather than dropping it.
 */

export const UNDO_WINDOW_MS = 6000;

type Run = () => void;

const pending = new Map<string, { timer: ReturnType<typeof setTimeout>; run: Run }>();

/** Run `run` after `delay` unless cancelled first. Replaces any earlier one for `id`. */
export const scheduleDelete = (
  id: string,
  run: Run,
  delay: number = UNDO_WINDOW_MS
) => {
  cancelDelete(id);
  const timer = setTimeout(() => {
    pending.delete(id);
    run();
  }, delay);
  pending.set(id, { timer, run });
};

/** Cancel a waiting delete. Whether one was waiting. */
export const cancelDelete = (id: string): boolean => {
  const entry = pending.get(id);
  if (!entry) return false;
  clearTimeout(entry.timer);
  pending.delete(id);
  return true;
};

/** Run every waiting delete now. */
export const flushDeletes = () => {
  const entries = [...pending.values()];
  pending.clear();
  entries.forEach(({ timer, run }) => {
    clearTimeout(timer);
    run();
  });
};

export const pendingDeleteCount = () => pending.size;

/** The request itself; `keepalive` lets it finish while the page unloads. */
export const sendDelete = (mailId: string) =>
  fetch(`/api/mails/${mailId}`, { method: "DELETE", keepalive: true }).catch(
    () => undefined
  );

if (typeof window !== "undefined") {
  window.addEventListener("pagehide", flushDeletes);
}
