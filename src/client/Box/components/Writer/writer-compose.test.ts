import {
  describe,
  it,
  expect,
  beforeAll,
  afterAll,
  beforeEach
} from "bun:test";
import { GlobalRegistrator } from "@happy-dom/global-registrator";
import { ReplyData } from "common";

// Compose behavior (design doc §7): pre-filled From, labeled Cc/Bcc,
// Discard, formatting toolbar, placeholder copy, editable/removable quote.
// Mounts the real Writer (same harness as `writer-listener.test.ts`).
beforeAll(() => {
  GlobalRegistrator.register();
  // React 18 only lets `act` flush work when this flag is set.
  (
    globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
});
afterAll(() => GlobalRegistrator.unregister());

beforeEach(() => window.localStorage.clear());

const { Editor } = await import("client/lib/editor");
const { selectAll } = await import("client/lib/editor/commands");

type EditorInstance = InstanceType<typeof Editor>;

const mountWriter = async (overrides: Record<string, unknown> = {}) => {
  const { createElement, act } = await import("react");
  const { createRoot } = await import("react-dom/client");
  const { QueryClient, QueryClientProvider } = await import("react-query");
  const { Context, Category } = await import("client");
  const Writer = (await import("./index")).default;

  // Captures the editor instance the Writer creates, for driving edits.
  let editor: EditorInstance | null = null;
  const originalOnUpdate = Editor.prototype.onUpdate;
  Editor.prototype.onUpdate = function (this: EditorInstance, fn: () => void) {
    if (!editor) editor = this;
    return originalOnUpdate.call(this, fn);
  };

  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } }
  });

  const contextValue = {
    domainName: "example.com",
    isWriterOpen: true,
    setIsWriterOpen: () => {},
    replyData: {},
    setReplyData: () => {},
    selectedAccount: "",
    setSelectedAccount: () => {},
    selectedCategory: Category.AllMails,
    setSelectedCategory: () => {},
    hasDraft: false,
    setHasDraft: () => {},
    ...overrides
  };

  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);

  const render = async () =>
    act(async () => {
      root.render(
        createElement(
          QueryClientProvider,
          { client: queryClient },
          createElement(
            Context.Provider,
            { value: contextValue as never },
            createElement(Writer)
          )
        )
      );
    });

  await render();

  return {
    act,
    container,
    editor: () => editor as unknown as EditorInstance,
    rerender: render,
    teardown: async () => {
      await act(async () => root.unmount());
      container.remove();
      Editor.prototype.onUpdate = originalOnUpdate;
    }
  };
};

const inputByPlaceholder = (container: HTMLElement, placeholder: string) =>
  container.querySelector(
    `input[placeholder="${placeholder}"]`
  ) as HTMLInputElement;

const setInputValue = (el: HTMLInputElement, value: string) => {
  // Drive the field the way real typing does: focus, then the value change
  // with an input event and a key event. React's change-event polyfill —
  // the path taken when react-dom was first evaluated before the DOM
  // globals existed, so the `input` event is treated as unsupported —
  // only notices a changed value on keyup/keydown of the focused element,
  // so the key event keeps this working in both modes (in the normal mode
  // the input event already fired onChange and the keyup is a no-op).
  el.focus();
  const setter = Object.getOwnPropertyDescriptor(
    window.HTMLInputElement.prototype,
    "value"
  )!.set!;
  setter.call(el, value);
  el.dispatchEvent(new Event("input", { bubbles: true }));
  el.dispatchEvent(
    new KeyboardEvent("keyup", {
      bubbles: true,
      key: value.slice(-1) || "a"
    })
  );
};

const buttonByLabel = (container: HTMLElement, label: string) =>
  container.querySelector(
    `button[aria-label="${label}"]`
  ) as HTMLButtonElement;

const buttonByText = (container: HTMLElement, text: string) =>
  Array.from(container.querySelectorAll("button")).find(
    (b) => b.textContent === text
  ) as HTMLButtonElement;

describe("Writer compose (design doc §7)", () => {
  it("pre-fills From from the selected mailbox on fresh compose", async () => {
    const { container, teardown } = await mountWriter({
      selectedAccount: "maple@example.com"
    });
    try {
      expect(inputByPlaceholder(container, "name").value).toBe("maple");
      expect(inputByPlaceholder(container, "account").value).toBe("maple");
    } finally {
      await teardown();
    }
  });

  it("does not pre-fill over an existing draft", async () => {
    window.localStorage.setItem("sender", JSON.stringify("fern"));
    window.localStorage.setItem("name", JSON.stringify("Fern"));
    const { container, teardown } = await mountWriter({
      selectedAccount: "maple@example.com"
    });
    try {
      expect(inputByPlaceholder(container, "name").value).toBe("Fern");
      expect(inputByPlaceholder(container, "account").value).toBe("fern");
    } finally {
      await teardown();
    }
  });

  it("does not pre-fill while searching, where the account holds a query", async () => {
    const { Category } = await import("client");
    const { container, teardown } = await mountWriter({
      selectedAccount: "invoice",
      selectedCategory: Category.Search
    });
    try {
      expect(inputByPlaceholder(container, "account").value).toBe("");
    } finally {
      await teardown();
    }
  });

  it("opens Cc and Bcc independently from labeled buttons", async () => {
    const { act, container, teardown } = await mountWriter();
    try {
      const cc = inputByPlaceholder(container, "cc-1@email.com, cc-2@email.com");
      const bcc = inputByPlaceholder(container, "bcc-1@email.com, bcc-2@email.com");
      expect(cc.disabled).toBe(true);
      expect(bcc.disabled).toBe(true);
      await act(async () => {
        buttonByText(container, "Cc").click();
      });
      expect(cc.disabled).toBe(false);
      expect(bcc.disabled).toBe(true);
      await act(async () => {
        buttonByText(container, "Bcc").click();
      });
      expect(bcc.disabled).toBe(false);
    } finally {
      await teardown();
    }
  });

  it("Discard clears the draft fields", async () => {
    const { act, container, teardown } = await mountWriter({
      selectedAccount: "maple@example.com"
    });
    try {
      const to = inputByPlaceholder(container, "to-1@email.com, to-2@email.com");
      const subject = inputByPlaceholder(container, "This is the mail subject");
      const name = inputByPlaceholder(container, "name");
      await act(async () => {
        setInputValue(to, "bob@example.com");
        setInputValue(subject, "Hello");
      });
      expect(to.value).toBe("bob@example.com");
      expect(name.value).toBe("maple");
      await act(async () => {
        buttonByText(container, "Discard").click();
      });
      // Discard clears the whole draft, including the pre-filled From name,
      // so the rail draft dot goes away too.
      expect(to.value).toBe("");
      expect(subject.value).toBe("");
      expect(name.value).toBe("");
    } finally {
      await teardown();
    }
  });

  it("draft dot stays off for pre-fill alone, lights on input, clears on Discard", async () => {
    const setHasDraftCalls: unknown[] = [];
    const { act, container, teardown } = await mountWriter({
      selectedAccount: "maple@example.com",
      setHasDraft: (v: unknown) => {
        setHasDraftCalls.push(v);
      }
    });
    try {
      // Fresh compose pre-fills From but the user typed nothing: no draft.
      expect(setHasDraftCalls[setHasDraftCalls.length - 1]).toBe(false);
      const to = inputByPlaceholder(container, "to-1@email.com, to-2@email.com");
      await act(async () => {
        setInputValue(to, "bob@example.com");
      });
      await act(async () => {});
      expect(setHasDraftCalls[setHasDraftCalls.length - 1]).toBe(true);
      await act(async () => {
        buttonByText(container, "Discard").click();
      });
      expect(setHasDraftCalls[setHasDraftCalls.length - 1]).toBe(false);
    } finally {
      await teardown();
    }
  });

  it("toolbar bolds the selected text and shows the pressed state", async () => {
    const { act, container, editor, teardown } = await mountWriter();
    try {
      await act(async () => {
        editor().insertText("hello");
        editor().run(selectAll);
      });
      const bold = buttonByLabel(container, "Bold");
      await act(async () => {
        bold.click();
      });
      expect(editor().getHTML()).toContain("<strong>hello</strong>");
      expect(bold.classList.contains("active")).toBe(true);
    } finally {
      await teardown();
    }
  });

  it("shows the new placeholder copy", async () => {
    const { container, teardown } = await mountWriter();
    try {
      expect(
        container.querySelector('[data-placeholder="Write your message…"]')
      ).not.toBeNull();
    } finally {
      await teardown();
    }
  });
});

describe("Writer reply quote", () => {
  const replyData: ReplyData = {
    id: "mail-1",
    messageId: "<abc@host>",
    date: "2026-01-02T03:04:05.000Z",
    subject: "Hello",
    from: {
      text: "Alice <alice@x.com>",
      value: [{ address: "alice@x.com", name: "Alice" }]
    } as ReplyData["from"],
    to: { address: "maple@example.com" } as ReplyData["to"],
    html: "<p>quoted body</p>"
  };

  it("renders the quoted body and removes it on Remove quote", async () => {
    const { act, container, teardown } = await mountWriter({ replyData });
    try {
      const quote = container.querySelector(".original_message");
      expect(quote).not.toBeNull();
      expect(quote!.textContent).toContain("quoted body");
      await act(async () => {
        buttonByLabel(container, "Remove quote").click();
      });
      expect(container.querySelector(".original_message")).toBeNull();
    } finally {
      await teardown();
    }
  });

  it("renders the original subject line in the quote", async () => {
    const { container, teardown } = await mountWriter({ replyData });
    try {
      const quote = container.querySelector(".original_message");
      expect(quote).not.toBeNull();
      const subject = quote!.querySelector(".quote-subject");
      expect(subject).not.toBeNull();
      expect(subject!.textContent).toBe("Hello");
    } finally {
      await teardown();
    }
  });

  it("renders no subject blockquote for a subject-less original", async () => {
    const { container, teardown } = await mountWriter({
      replyData: { ...replyData, subject: "" }
    });
    try {
      const quote = container.querySelector(".original_message");
      expect(quote).not.toBeNull();
      expect(quote!.querySelector(".quote-subject")).toBeNull();
    } finally {
      await teardown();
    }
  });

  it("keeps edited quote text across a re-render", async () => {
    const { act, container, rerender, teardown } = await mountWriter({
      replyData
    });
    try {
      const body = container.querySelector(
        ".original_message_body"
      ) as HTMLDivElement;
      expect(body.textContent).toContain("quoted body");
      await act(async () => {
        body.innerHTML = "<p>edited quote</p>";
        body.dispatchEvent(new Event("input", { bubbles: true }));
      });
      await rerender();
      expect(body.innerHTML).toContain("edited quote");
    } finally {
      await teardown();
    }
  });

  it("clicking the quote does not close the writer", async () => {
    const setIsWriterOpenCalls: unknown[] = [];
    const { act, container, teardown } = await mountWriter({
      replyData,
      setIsWriterOpen: (v: unknown) => {
        setIsWriterOpenCalls.push(v);
      }
    });
    try {
      const quote = container.querySelector(
        ".original_message"
      ) as HTMLDivElement;
      await act(async () => {
        quote.click();
      });
      expect(setIsWriterOpenCalls).toEqual([]);
      expect(container.querySelector(".original_message")).not.toBeNull();
    } finally {
      await teardown();
    }
  });
});
