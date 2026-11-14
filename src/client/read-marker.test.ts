// Opening a mail left its red unread dot on the row until the page was
// refreshed (Hoie, 2026-11-14). The list's own mark-read edit copied the
// array but mutated the mail object in place, so react-query's structural
// sharing handed back the previous data reference, the rows memo never
// re-derived, and the dot stayed — even though the server had marked the
// mail read (a refresh fetched fresh objects and the dot cleared).
//
// These tests mount the real App (same harness shape as
// search-reentry.test.ts, with `call.get`/`call.post` mocked) and drive a
// row open and back. The thread mock mirrors the server: the mark POST
// lands before the thread GET, so the thread data already says read — which
// is what makes the Thread's own mirror effect a no-op and leaves the list
// edit as the only thing that can clear the dot.
import "fake-indexeddb/auto";
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
import { Account, SignedUser } from "common";
import { Category, call, queryClient } from "client";

beforeAll(() => {
  GlobalRegistrator.register();
  (
    globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
});
afterAll(() => GlobalRegistrator.unregister());

const address = (name: string, email: string) => ({
  name,
  address: email
});

const mailOne = {
  id: "mail-1",
  subject: "Weekend tasting event",
  date: new Date("2026-11-07T10:00:00Z").toISOString(),
  read: false,
  saved: false,
  sent: false,
  from: {
    value: [address("Harbor Coffee", "harbor@example.com")],
    text: "Harbor Coffee"
  },
  to: { value: [address("Me", "maple@example.com")], text: "Me" },
  snippet: "Saturday at 10 works for us"
};

const mailTwo = {
  id: "mail-2",
  subject: "Invoice #4812 is due Friday",
  date: new Date("2026-11-06T10:00:00Z").toISOString(),
  read: true,
  saved: true,
  sent: false,
  from: {
    value: [address("Northwind Traders", "northwind@example.com")],
    text: "Northwind Traders"
  },
  to: { value: [address("Me", "maple@example.com")], text: "Me" },
  snippet: "A reminder that invoice 4812 is due this Friday."
};

const accountsPayload = {
  received: [
    new Account({
      key: "maple@example.com",
      updated: new Date("2026-01-01"),
      doc_count: 5,
      unread_doc_count: 3,
      saved_doc_count: 1
    })
  ],
  sent: [],
  spam: []
};

// Whether the mark POST has landed — the thread mock reads it, the way the
// server's thread query reads the row the POST just updated.
let markedRead = false;

let originalGet: typeof call.get;
let originalPost: typeof call.post;

beforeEach(() => {
  window.localStorage.clear();
  queryClient.clear();
  markedRead = false;
  originalGet = call.get;
  originalPost = call.post;
  call.get = (async (url: string) => {
    if (url.startsWith("/api/mails/domain"))
      return { status: "success", body: "example.com" };
    if (url.startsWith("/api/mails/search-accounts/"))
      return { status: "success", body: accountsPayload.received };
    if (url.startsWith("/api/mails/headers/"))
      return { status: "success", body: [mailOne, mailTwo] };
    if (url.startsWith("/api/mails/accounts"))
      return { status: "success", body: accountsPayload };
    if (url.startsWith("/api/mails/thread/"))
      return {
        status: "success",
        body: [{ ...mailOne, read: markedRead }]
      };
    if (url.startsWith("/api/mails/body/"))
      return {
        status: "success",
        body: { id: mailOne.id, html: "<p>body</p>", messageId: "<m1@x>" }
      };
    return { status: "success", body: [] };
  }) as typeof call.get;
  call.post = (async (url: string) => {
    if (url.startsWith("/api/mails/mark")) markedRead = true;
    return { status: "success", body: undefined };
  }) as typeof call.post;
});

afterEach(() => {
  call.get = originalGet;
  call.post = originalPost;
});

const mountApp = async () => {
  window.localStorage.setItem(
    "selectedAccount",
    JSON.stringify("maple@example.com")
  );
  window.localStorage.setItem(
    "selectedCategory",
    JSON.stringify(Category.AllMails)
  );
  window.localStorage.setItem("isAccountsOpen", JSON.stringify(true));
  window.localStorage.setItem("isWriterOpen", JSON.stringify(false));

  const { createElement, act } = await import("react");
  const { createRoot } = await import("react-dom/client");
  const { App } = await import("client");

  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);

  (
    window as unknown as {
      happyDOM: { setURL: (url: string) => void };
    }
  ).happyDOM.setURL("http://localhost/");

  await act(async () => {
    root.render(
      createElement(App, {
        user: new SignedUser({
          id: "user-1",
          email: "maple@example.com",
          username: "maple"
        })
      })
    );
  });

  const waitFor = async (fn: () => Element | null) => {
    for (let i = 0; i < 100; i++) {
      const el = fn();
      if (el) return el;
      await act(async () => {
        await new Promise((r) => setTimeout(r, 10));
      });
    }
    throw new Error("waitFor timed out");
  };

  const click = async (el: Element) => {
    await act(async () => {
      (el as HTMLElement).click();
    });
    for (let i = 0; i < 5; i++) {
      await act(async () => {
        await new Promise((r) => setTimeout(r, 0));
      });
    }
  };

  return {
    container,
    act,
    waitFor,
    click,
    mailCards: () => Array.from(container.querySelectorAll(".mailcard")),
    threadBack: () =>
      container.querySelector(".thread-back") as HTMLElement | null,
    teardown: async () => {
      await act(async () => root.unmount());
      container.remove();
      queryClient.clear();
    }
  };
};

describe("opening a mail clears its unread dot without a refresh", () => {
  it("the row loses the unread dot after opening and going back", async () => {
    const mount = await mountApp();
    try {
      await mount.waitFor(() => mount.mailCards()[0] ?? null);
      const card = mount
        .mailCards()
        .find((c) => c.textContent?.includes("Weekend tasting event"))!;
      expect(card.querySelector(".unread-dot")).not.toBeNull();

      // Open the mail, then go back to the list — no refresh anywhere. The
      // row's click handler lives on the inner header element (MailHeader),
      // not the .mailcard blockquote, so click that.
      const rowHeader = card.querySelector(".header.cursor")!;
      await mount.click(rowHeader);
      await mount.waitFor(() => mount.threadBack());
      await mount.click(mount.threadBack()!);

      // The dot must clear on the cached list, not only after a refetch.
      let dotGone = false;
      for (let i = 0; i < 100; i++) {
        const current = mount
          .mailCards()
          .find((c) => c.textContent?.includes("Weekend tasting event"));
        if (current && !current.querySelector(".unread-dot")) {
          dotGone = true;
          break;
        }
        await mount.act(async () => {
          await new Promise((r) => setTimeout(r, 10));
        });
      }
      expect(dotGone).toBe(true);
    } finally {
      await mount.teardown();
    }
  });
});
