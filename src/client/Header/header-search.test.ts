import {
  describe,
  it,
  expect,
  beforeAll,
  afterAll
} from "bun:test";
import { GlobalRegistrator } from "@happy-dom/global-registrator";
import { Category } from "client";

// Header (design doc §4/§9): the title and the unread count pill next to it.
beforeAll(() => {
  GlobalRegistrator.register();
  (
    globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
});
afterAll(() => GlobalRegistrator.unregister());

type MountOptions = {
  newMailsTotal?: number;
  userInfo?: { username: string } | undefined;
  selectedAccount?: string;
  selectedCategory?: Category;
  isSettingsOpen?: boolean;
  accounts?: unknown;
};

const mountHeader = async (options: MountOptions = {}) => {
  const { newMailsTotal = 0, selectedAccount = "maple@example.com" } = options;
  // NB: destructuring defaults apply to an explicit `undefined` too, so a
  // signed-out mount (userInfo: undefined) needs the `in` check — the
  // default only applies when the key is absent.
  const userInfo =
    "userInfo" in options ? options.userInfo : { username: "maple" };

  const { createElement, act } = await import("react");
  const { createRoot } = await import("react-dom/client");
  const { Context } = await import("client");
  const { QueryClient, QueryClientProvider } = await import("react-query");
  const Header = (await import("./index")).default;
  const queryClient = new QueryClient();
  if (options.accounts) {
    queryClient.setQueryData("/api/mails/accounts", options.accounts);
  }

  const enterSearchCalls: number[] = [];

  const contextValue = {
    viewSize: { width: 1920, height: 1080 },
    domainName: "example.com",
    userInfo,
    setUserInfo: () => {},
    isAccountsOpen: true,
    setIsAccountsOpen: () => {},
    isWriterOpen: false,
    setIsWriterOpen: () => {},
    replyData: {},
    setReplyData: () => {},
    selectedAccount,
    setSelectedAccount: () => {},
    selectedCategory: options.selectedCategory ?? Category.AllMails,
    setSelectedCategory: () => {},
    isSettingsOpen: options.isSettingsOpen ?? false,
    setIsSettingsOpen: () => {},
    newMailsTotal,
    setNewMailsTotal: () => {},
    preSearchAccount: { current: "" },
    preSearchCategory: { current: Category.AllMails },
    enterSearch: () => {
      enterSearchCalls.push(1);
    },
    exitSearch: () => {},
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
        Context.Provider,
        { value: contextValue as never },
        createElement(
          QueryClientProvider,
          { client: queryClient },
          createElement(Header)
        )
      )
    );
  });

  return {
    container,
    enterSearchCalls,
    teardown: async () => {
      await act(async () => root.unmount());
      container.remove();
    }
  };
};

const account = (key: string, unread: number) => ({
  key,
  updated: "2026-01-01T00:00:00.000Z",
  doc_count: 5,
  unread_doc_count: unread,
  saved_doc_count: 0
});

describe("header unread count", () => {
  it("shows the selected account's own unread count next to its name", async () => {
    const mount = await mountHeader({
      newMailsTotal: 27,
      selectedAccount: "maple@example.com",
      accounts: {
        received: [account("maple@example.com", 3), account("fern@example.com", 24)],
        sent: [],
        spam: []
      }
    });
    try {
      const pill = mount.container.querySelector(".header-unread-count");
      expect(mount.container.querySelector("h1")?.textContent).toBe(
        "maple@example.com"
      );
      expect(pill?.textContent).toBe("3");
      expect(pill?.getAttribute("title")).toBe("3 unread in maple@example.com");
    } finally {
      await mount.teardown();
    }
  });

  it("shows the all-accounts total when no account is selected", async () => {
    const mount = await mountHeader({ newMailsTotal: 4, selectedAccount: "" });
    try {
      const pill = mount.container.querySelector(".header-unread-count");
      expect(pill?.textContent).toBe("4");
      expect(pill?.getAttribute("title")).toBe("4 unread in all accounts");
    } finally {
      await mount.teardown();
    }
  });

  it("titles search by its mode, not by the search term", async () => {
    const mount = await mountHeader({
      newMailsTotal: 4,
      selectedAccount: "invoice",
      selectedCategory: Category.Search
    });
    try {
      expect(mount.container.querySelector("h1")?.textContent).toBe("Search");
      expect(
        mount.container.querySelector(".header-unread-count")?.getAttribute("title")
      ).toBe("4 unread in all accounts");
    } finally {
      await mount.teardown();
    }
  });

  it("titles the Settings page Settings", async () => {
    const mount = await mountHeader({ isSettingsOpen: true });
    try {
      expect(mount.container.querySelector("h1")?.textContent).toBe("Settings");
    } finally {
      await mount.teardown();
    }
  });

  it("shows no pill when nothing is unread", async () => {
    const mount = await mountHeader({ newMailsTotal: 0, selectedAccount: "" });
    try {
      expect(
        mount.container.querySelector(".header-unread-count")
      ).toBeNull();
    } finally {
      await mount.teardown();
    }
  });

  it("shows no pill before sign-in", async () => {
    const mount = await mountHeader({
      newMailsTotal: 4,
      selectedAccount: "",
      userInfo: undefined
    });
    try {
      expect(
        mount.container.querySelector(".header-unread-count")
      ).toBeNull();
    } finally {
      await mount.teardown();
    }
  });
});

describe("header controls", () => {
  // Search and Compose are always on screen in the rail (the bottom bar on
  // a phone), so the header does not repeat them.
  it("has no second search or compose trigger", async () => {
    const mount = await mountHeader();
    try {
      expect(mount.container.querySelector('[aria-label="Search"]')).toBeNull();
      expect(
        mount.container.querySelector('[aria-label="Compose new mail"]')
      ).toBeNull();
    } finally {
      await mount.teardown();
    }
  });
});
