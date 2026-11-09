import {
  describe,
  it,
  expect,
  beforeAll,
  afterAll
} from "bun:test";
import { GlobalRegistrator } from "@happy-dom/global-registrator";
import { Category } from "client";

// Header (design doc §4/§9): the unread count pill next to the title, and
// the search trigger that opens the center-column search box.
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
  const Header = (await import("./index")).default;

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
    selectedCategory: Category.AllMails,
    setSelectedCategory: () => {},
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
        createElement(Header)
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

describe("header unread count", () => {
  it("shows the total unread count next to the title", async () => {
    const mount = await mountHeader({ newMailsTotal: 4 });
    try {
      const pill = mount.container.querySelector(".header-unread-count");
      expect(pill?.textContent).toBe("4");
      expect(pill?.getAttribute("title")).toBe("4 unread");
    } finally {
      await mount.teardown();
    }
  });

  it("shows no pill when nothing is unread", async () => {
    const mount = await mountHeader({ newMailsTotal: 0 });
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

describe("header search trigger", () => {
  it("clicking the search trigger enters search", async () => {
    const mount = await mountHeader();
    try {
      const trigger = mount.container.querySelector(
        '.menu.right [aria-label="Search"]'
      ) as HTMLElement;
      expect(trigger).not.toBeNull();
      const { act } = await import("react");
      await act(async () => {
        trigger.click();
      });
      expect(mount.enterSearchCalls.length).toBe(1);
    } finally {
      await mount.teardown();
    }
  });

  it("the compose trigger still works alongside the search trigger", async () => {
    const mount = await mountHeader();
    try {
      const triggers = mount.container.querySelectorAll(".menu.right .cursor");
      expect(triggers.length).toBe(2);
      expect(
        (triggers[1] as HTMLElement).getAttribute("aria-label")
      ).toBe("Compose new mail");
    } finally {
      await mount.teardown();
    }
  });
});
