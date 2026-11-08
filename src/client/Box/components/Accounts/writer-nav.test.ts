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
import { Account } from "common";
import { Category, call } from "client";
import path from "path";

// Navigating while the writer is open used to leave the writer covering the
// screen — the taps reached the rail and the list underneath switched, but
// nothing closed the writer (design doc §4; the fix is in `onClickCategory`).
// These tests mount the real Accounts component (same harness as
// `accounts-logout.test.ts`) and drive the rail.
beforeAll(() => {
  GlobalRegistrator.register();
  // React 18 only lets `act` flush work when this flag is set.
  (
    globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
});
afterAll(() => GlobalRegistrator.unregister());

const accountsPayload = {
  received: [
    new Account({
      key: "maple@example.com",
      updated: new Date("2026-01-01"),
      doc_count: 1,
      unread_doc_count: 0,
      saved_doc_count: 0
    })
  ],
  sent: [],
  spam: []
};

let originalGet: typeof call.get;

beforeEach(() => {
  window.localStorage.clear();
  originalGet = call.get;
  call.get = (async () => ({
    status: "success",
    body: accountsPayload
  })) as typeof call.get;
});

afterEach(() => {
  call.get = originalGet;
});

type MountOptions = {
  width?: number;
  writerOpen?: boolean;
  accountsOpen?: boolean;
  category?: Category;
};

const mountAccounts = async (options: MountOptions = {}) => {
  const {
    width = 1920,
    writerOpen = false,
    accountsOpen = true,
    category = Category.AllMails
  } = options;

  const { createElement, act } = await import("react");
  const { createRoot } = await import("react-dom/client");
  const { QueryClient, QueryClientProvider } = await import("react-query");
  const { Context, IsOnlineProvider } = await import("client");
  const Accounts = (await import("./index")).default;

  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } }
  });

  // The setters are recorded, not stubbed away: the fix under test is
  // exactly which setters `onClickCategory` calls.
  const writerOpenCalls: boolean[] = [];
  const accountsOpenCalls: boolean[] = [];
  const categoryCalls: Category[] = [];

  // Minimum context `Accounts` reads.
  const contextValue = {
    viewSize: { width, height: 1080 },
    domainName: "example.com",
    userInfo: undefined,
    setUserInfo: () => {},
    isAccountsOpen: accountsOpen,
    setIsAccountsOpen: (value: boolean) => {
      accountsOpenCalls.push(value);
    },
    isWriterOpen: writerOpen,
    setIsWriterOpen: (value: boolean) => {
      writerOpenCalls.push(value);
    },
    replyData: {},
    setReplyData: () => {},
    selectedAccount: "",
    setSelectedAccount: () => {},
    selectedCategory: category,
    setSelectedCategory: (value: Category) => {
      categoryCalls.push(value);
    },
    newMailsTotal: 0,
    setNewMailsTotal: () => {},
    density: "comfortable",
    setDensity: () => {},
    hasDraft: false,
    setHasDraft: () => {}
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
            createElement(Accounts, { setPage: () => {} })
          )
        )
      )
    );
  });

  // The rail renders once the accounts query resolves; poll until it is
  // there (or fail loudly — a missing rail is the finding).
  const waitForRail = async () => {
    for (let i = 0; i < 100; i++) {
      const items = container.querySelectorAll(".categories .rail-item");
      if (items.length > 0) return items;
      await act(async () => {
        await new Promise((r) => setTimeout(r, 10));
      });
    }
    throw new Error("Rail items never rendered");
  };

  const clickRailItem = async (label: string) => {
    const items = await waitForRail();
    const item = Array.from(items).find((el) =>
      el.textContent?.includes(label)
    ) as HTMLElement;
    expect(item).not.toBeUndefined();
    await act(async () => {
      item.click();
    });
    for (let i = 0; i < 5; i++) {
      await act(async () => {
        await new Promise((r) => setTimeout(r, 0));
      });
    }
  };

  return {
    container,
    writerOpenCalls,
    accountsOpenCalls,
    categoryCalls,
    clickRailItem,
    teardown: async () => {
      await act(async () => root.unmount());
      container.remove();
      queryClient.clear();
    }
  };
};

describe("Writer closes on navigation", () => {
  it("tapping a folder with the writer open closes the writer", async () => {
    const { writerOpenCalls, categoryCalls, clickRailItem, teardown } =
      await mountAccounts({ writerOpen: true });
    try {
      await clickRailItem("Saved");
      expect(categoryCalls).toContain(Category.SavedMails);
      expect(writerOpenCalls).toContain(false);
    } finally {
      await teardown();
    }
  });

  it("tapping a folder with the writer closed does not reopen it", async () => {
    const { writerOpenCalls, clickRailItem, teardown } = await mountAccounts({
      writerOpen: false
    });
    try {
      await clickRailItem("Saved");
      expect(writerOpenCalls).not.toContain(true);
    } finally {
      await teardown();
    }
  });
});

describe("Search opens the accounts panel on narrow screens", () => {
  it("tapping Search at 390px opens the panel and closes the writer", async () => {
    const { writerOpenCalls, accountsOpenCalls, clickRailItem, teardown } =
      await mountAccounts({
        width: 390,
        writerOpen: true,
        accountsOpen: false
      });
    try {
      await clickRailItem("Search");
      expect(accountsOpenCalls).toContain(true);
      expect(writerOpenCalls).toContain(false);
    } finally {
      await teardown();
    }
  });

  it("tapping Search at 1920px does not force the panel open", async () => {
    const { accountsOpenCalls, clickRailItem, teardown } = await mountAccounts(
      {
        width: 1920,
        writerOpen: false,
        accountsOpen: false
      }
    );
    try {
      await clickRailItem("Search");
      expect(accountsOpenCalls).toEqual([]);
    } finally {
      await teardown();
    }
  });
});

describe("Closed writer does not block clicks", () => {
  it("the closed pane is visibility:hidden with an opacity fade, not a slide", async () => {
    // happy-dom has no layout engine, so the hide rules themselves are
    // pinned here in the stylesheet: the closed pane is invisible AND out
    // of the hit-test tree, and the open pane fades in. The sandbox
    // screenshots verify the rendered result at both widths.
    const scss = await Bun.file(
      path.join(import.meta.dir, "../../index.scss")
    ).text();
    expect(scss).toMatch(
      /\.writer-pane\s*\{[^}]*?opacity:\s*0;/s
    );
    expect(scss).toMatch(
      /\.writer-pane\s*\{[^}]*?visibility:\s*hidden;/s
    );
    expect(scss).toMatch(
      /\.writer-pane\s*\{[^}]*?&\.open\s*\{[^}]*?opacity:\s*1;/s
    );
    expect(scss).toMatch(
      /\.writer-pane\s*\{[^}]*?&\.open\s*\{[^}]*?visibility:\s*visible;/s
    );
    // The old hide mechanism was a transform slide — a closed pane
    // translated off-screen still covers the right 600px in the hit-test
    // tree if the transform is ever dropped. No translateX remains.
    expect(scss).not.toContain("translateX");
  });
});
