import {
  describe,
  it,
  expect,
  beforeAll,
  afterAll,
  beforeEach,
  afterEach
} from "bun:test";
import { GlobalRegistrator } from "@happy-dom/global-registrator";
import { Category, call } from "client";

// A folder lists conversations, one row each (conversations.ts); search
// lists the matching messages themselves. Mounts the real Mails list.
beforeAll(() => {
  GlobalRegistrator.register();
  (
    globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
});
afterAll(() => GlobalRegistrator.unregister());

const address = (name: string, email: string) => ({ name, address: email });

const message = (
  id: string,
  day: number,
  fields: Record<string, unknown> = {}
) => ({
  id,
  messageId: `<${id}@x>`,
  subject: "Budget",
  date: new Date(`2026-11-0${day}T10:00:00Z`).toISOString(),
  read: true,
  saved: false,
  sent: false,
  from: { value: [address("Alice", "alice@x.com")], text: "Alice" },
  to: { value: [address("Me", "maple@example.com")], text: "Me" },
  snippet: `snippet ${id}`,
  ...fields
});

// Two messages of one conversation (the second answers the first) and an
// unrelated one.
const list = [
  message("m3", 3, {
    subject: "Re: Budget",
    inReplyTo: "<m1@x>",
    references: ["<m1@x>"],
    read: false,
    from: { value: [address("Bob", "bob@x.com")], text: "Bob" }
  }),
  message("other", 2, { subject: "Lunch" }),
  message("m1", 1)
];

let originalGet: typeof call.get;
let originalFetch: typeof fetch;
let deleted: string[];

beforeEach(() => {
  window.localStorage.clear();
  deleted = [];
  originalGet = call.get;
  originalFetch = globalThis.fetch;
  call.get = (async (url: string) => {
    if (url.startsWith("/api/mails/search/")) {
      return { status: "success", body: [list[0], list[2]] };
    }
    if (url.startsWith("/api/mails/headers/")) {
      return { status: "success", body: list };
    }
    return { status: "success", body: [] };
  }) as typeof call.get;
  globalThis.fetch = (async (url: string) => {
    deleted.push(url);
    return new Response("{}");
  }) as unknown as typeof fetch;
});

afterEach(() => {
  call.get = originalGet;
  globalThis.fetch = originalFetch;
});

const mountMails = async (category = Category.AllMails, selected = "maple@example.com") => {
  const { createElement, act } = await import("react");
  const { createRoot } = await import("react-dom/client");
  const { QueryClientProvider } = await import("react-query");
  const { MemoryRouter } = await import("react-router-dom");
  // The app's own client: the list's optimistic edits (MailsCache) write to it.
  const { Context, IsOnlineProvider, queryClient } = await import("client");
  const Mails = (await import("./index")).default;

  queryClient.clear();
  const contextValue = {
    viewSize: { width: 1920, height: 1080 },
    domainName: "example.com",
    userInfo: undefined,
    selectedAccount: selected,
    setSelectedAccount: () => {},
    selectedCategory: category,
    setSelectedCategory: () => {},
    preSearchAccount: { current: "maple@example.com" },
    preSearchCategory: { current: Category.AllMails },
    enterSearch: () => {},
    exitSearch: () => {},
    setReplyData: () => {},
    setIsWriterOpen: () => {},
    density: "comfortable"
  };

  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  await act(async () => {
    root.render(
      createElement(
        QueryClientProvider,
        { client: queryClient },
        createElement(
          IsOnlineProvider,
          null,
          createElement(
            Context.Provider,
            { value: contextValue as never },
            createElement(MemoryRouter, null, createElement(Mails, { page: 0 }))
          )
        )
      )
    );
  });

  const waitFor = async <T>(fn: () => T | null | undefined): Promise<T> => {
    for (let i = 0; i < 100; i++) {
      const value = fn();
      if (value) return value;
      await act(async () => {
        await new Promise((r) => setTimeout(r, 10));
      });
    }
    throw new Error("waitFor timed out");
  };

  await waitFor(() => container.querySelector(".mailcard"));
  const rows = () => Array.from(container.querySelectorAll(".mailcard"));

  return {
    container,
    act,
    waitFor,
    rows,
    teardown: async () => {
      await act(async () => root.unmount());
      container.remove();
      queryClient.clear();
    }
  };
};

describe("folder rows are conversations", () => {
  it("shows a conversation as one row with its senders and count", async () => {
    const mount = await mountMails();
    try {
      const rows = mount.rows();
      expect(rows.length).toBe(2);
      const first = rows[0];
      expect(first.querySelector(".sender")?.textContent).toBe("Alice, Bob");
      expect(first.querySelector(".thread-count-badge")?.textContent).toBe("2");
      // The newest message's subject; unread because one message is.
      expect(first.querySelector(".mailcard-subject")?.textContent).toBe(
        "Re: Budget"
      );
      expect(first.classList.contains("unread")).toBe(true);
      expect(rows[1].querySelector(".thread-count-badge")).toBeNull();
    } finally {
      await mount.teardown();
    }
  });

  it("deletes every message of the conversation under one Undo window", async () => {
    const { flushDeletes, pendingDeleteCount } = await import("./pendingDeletes");
    const mount = await mountMails();
    try {
      const first = mount.rows()[0];
      await mount.act(async () => {
        (first.querySelector('[aria-label="More actions"]') as HTMLElement).click();
      });
      await mount.act(async () => {
        (
          mount.container.querySelector('[aria-label="Delete"]') as HTMLElement
        ).click();
      });
      expect(mount.rows().length).toBe(1);
      expect(pendingDeleteCount()).toBe(2);
      expect(deleted).toEqual([]);
      flushDeletes();
      expect(deleted.sort()).toEqual(["/api/mails/m1", "/api/mails/m3"]);
    } finally {
      await mount.teardown();
    }
  });

  it("lists search matches one by one", async () => {
    const mount = await mountMails(Category.Search, "budget");
    try {
      expect(mount.rows().length).toBe(2);
      expect(mount.container.querySelector(".thread-count-badge")).toBeNull();
    } finally {
      await mount.teardown();
    }
  });
});
