import {
  createContext,
  ReactNode,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState
} from "react";
import "./feedback.scss";

/**
 * In-app feedback: short toasts (optionally with one action, such as Undo)
 * and a confirm dialog. They replace the browser's `alert`/`confirm`, which
 * block the page, can't be styled to the dark theme, and offer no undo.
 */

export interface ToastOptions {
  message: string;
  /** One action button on the toast, e.g. Undo. */
  action?: { label: string; onClick: () => void };
  /** Milliseconds before the toast goes away by itself. */
  duration?: number;
  tone?: "info" | "error";
}

export interface ConfirmOptions {
  title: string;
  message?: string;
  confirmLabel?: string;
  cancelLabel?: string;
  /** Styles the confirm button as destructive. */
  danger?: boolean;
}

export interface FeedbackApi {
  showToast: (toast: ToastOptions) => void;
  confirm: (options: ConfirmOptions) => Promise<boolean>;
}

/**
 * Without a provider (component tests that mount one piece of the app),
 * toasts are dropped and confirm falls back to the browser's dialog, so the
 * caller's flow still runs.
 */
const fallback: FeedbackApi = {
  showToast: () => {},
  confirm: ({ title, message }) =>
    Promise.resolve(
      window.confirm(message ? `${title}\n\n${message}` : title)
    )
};

const FeedbackContext = createContext<FeedbackApi>(fallback);

export const useFeedback = () => useContext(FeedbackContext);

interface ActiveToast extends ToastOptions {
  id: number;
}

interface PendingConfirm extends ConfirmOptions {
  resolve: (value: boolean) => void;
}

const DEFAULT_TOAST_MS = 5000;

const ConfirmDialog = ({
  pending,
  onClose
}: {
  pending: PendingConfirm;
  onClose: (value: boolean) => void;
}) => {
  const confirmRef = useRef<HTMLButtonElement>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    // Focus the safe choice for a destructive action, the action otherwise.
    (pending.danger ? cancelRef : confirmRef).current?.focus();
  }, [pending]);

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        onClose(false);
      } else if (e.key === "Tab") {
        // Keep focus on the dialog's two buttons while it is open.
        const buttons = [cancelRef.current, confirmRef.current].filter(
          (b): b is HTMLButtonElement => !!b
        );
        const index = buttons.indexOf(
          document.activeElement as HTMLButtonElement
        );
        e.preventDefault();
        const next = e.shiftKey
          ? buttons[(index - 1 + buttons.length) % buttons.length]
          : buttons[(index + 1) % buttons.length];
        next?.focus();
      }
    };
    // Capture, so the dialog sees Escape before the views behind it.
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, [onClose]);

  return (
    <div className="feedback-backdrop" onClick={() => onClose(false)}>
      <div
        className="feedback-dialog"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="feedback-dialog-title"
        aria-describedby={
          pending.message ? "feedback-dialog-message" : undefined
        }
        onClick={(e) => e.stopPropagation()}
      >
        <h2 id="feedback-dialog-title">{pending.title}</h2>
        {pending.message ? (
          <p id="feedback-dialog-message">{pending.message}</p>
        ) : null}
        <div className="feedback-dialog-buttons">
          <button
            type="button"
            className="feedback-button"
            ref={cancelRef}
            onClick={() => onClose(false)}
          >
            {pending.cancelLabel ?? "Cancel"}
          </button>
          <button
            type="button"
            className={
              "feedback-button primary" + (pending.danger ? " danger" : "")
            }
            ref={confirmRef}
            onClick={() => onClose(true)}
          >
            {pending.confirmLabel ?? "OK"}
          </button>
        </div>
      </div>
    </div>
  );
};

export const FeedbackProvider = ({ children }: { children: ReactNode }) => {
  const [toasts, setToasts] = useState<ActiveToast[]>([]);
  const [pending, setPending] = useState<PendingConfirm | null>(null);
  const nextId = useRef(1);
  const timers = useRef(new Map<number, ReturnType<typeof setTimeout>>());

  const dismiss = useCallback((id: number) => {
    clearTimeout(timers.current.get(id));
    timers.current.delete(id);
    setToasts((all) => all.filter((t) => t.id !== id));
  }, []);

  const showToast = useCallback(
    (toast: ToastOptions) => {
      const id = nextId.current++;
      // One toast at a time: a new one replaces the last.
      setToasts((all) => {
        all.forEach((t) => {
          clearTimeout(timers.current.get(t.id));
          timers.current.delete(t.id);
        });
        return [{ ...toast, id }];
      });
      timers.current.set(
        id,
        setTimeout(() => dismiss(id), toast.duration ?? DEFAULT_TOAST_MS)
      );
    },
    [dismiss]
  );

  const confirm = useCallback(
    (options: ConfirmOptions) =>
      new Promise<boolean>((resolve) => {
        // Hand focus back to whatever opened the dialog once it closes.
        const opener = document.activeElement as HTMLElement | null;
        setPending({
          ...options,
          resolve: (value) => {
            opener?.focus?.();
            resolve(value);
          }
        });
      }),
    []
  );

  useEffect(() => {
    const all = timers.current;
    return () => all.forEach((t) => clearTimeout(t));
  }, []);

  const pendingRef = useRef<PendingConfirm | null>(null);
  pendingRef.current = pending;
  const onCloseDialog = useCallback((value: boolean) => {
    const current = pendingRef.current;
    pendingRef.current = null;
    setPending(null);
    current?.resolve(value);
  }, []);

  return (
    <FeedbackContext.Provider value={{ showToast, confirm }}>
      {children}
      <div className="feedback-toasts" role="status" aria-live="polite">
        {toasts.map((t) => (
          <div
            key={t.id}
            className={"feedback-toast" + (t.tone === "error" ? " error" : "")}
          >
            <span>{t.message}</span>
            {t.action ? (
              <button
                type="button"
                className="feedback-toast-action"
                onClick={() => {
                  t.action?.onClick();
                  dismiss(t.id);
                }}
              >
                {t.action.label}
              </button>
            ) : null}
            <button
              type="button"
              className="feedback-toast-close"
              aria-label="Dismiss"
              onClick={() => dismiss(t.id)}
            >
              ×
            </button>
          </div>
        ))}
      </div>
      {pending ? (
        <ConfirmDialog pending={pending} onClose={onCloseDialog} />
      ) : null}
    </FeedbackContext.Provider>
  );
};
