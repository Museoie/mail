/**
 * `persistToSentMailbox: false` is what keeps a live bearer credential out of
 * the sending identity's Sent mailbox: anyone with the viewer role on the
 * sending domain reads it, so a Sent record there would hand them the
 * credential.
 *
 * The property is "`sendMail`, given that option, writes no Sent record" — not
 * "the caller passes an option object", which a mocked `sendMail` is all that
 * can assert. These drive the real one and assert on `saveMail` itself.
 */
import { describe, it, expect, mock, spyOn, beforeEach, afterAll } from "bun:test";
import { MailDataToSend, SignedUser } from "common";
import * as mailgunModule from "./mailgun";
import * as domainRepo from "../postgres/repositories/domains";
import * as alarmModule from "../alarm";

const REAL_SERVER = (globalThis as Record<string, unknown>).__REAL_SERVER as Record<
  string,
  unknown
>;

// Snapshot before mocking: the imported namespace object is live, and Bun
// mutates it in place when the mock installs, so restoring from it hands the
// stub back and leaves the module mocked for every later file in the run.
// Which file runs first is filesystem enumeration order, and that differs
// between the CI and CD commands.
const REAL_MAILGUN = { ...mailgunModule };

const mockSaveMail = mock(async () => undefined);
const mockSendMailgunMail = mock(async () => ({ id: "mailgun-message-id" }));

mock.module("server", () => ({
  ...REAL_SERVER,
  saveMail: mockSaveMail,
  getUserDomain: (username: string) => `${username}.example.com`,
  getText: (html: string) => html,
  saveBuffer: async () => "buffer-id",
  getDomainUidNext: async () => 1,
  getAccountUidNext: async () => 1,
}));

mock.module("./mailgun", () => ({
  ...REAL_MAILGUN,
  sendMailgunMail: mockSendMailgunMail,
}));

// The domains the sender owns; spied so no database is reached.
const ownedAccess = (name: string): domainRepo.DomainAccess => ({
  domainId: `d-${name}`,
  name,
  ownerId: "11111111-2222-3333-4444-555555555555",
  ownerUsername: "admin",
  role: "owner",
  verifiedAt: null,
});
const mockListDomainAccess = spyOn(domainRepo, "listDomainAccess").mockResolvedValue([
  ownedAccess("example.com"),
  ownedAccess("example.org"),
  { ...ownedAccess("example.net"), role: "viewer", ownerId: "someone-else" },
]);

const { sendMail, sendSystemMail } = await import("./send");
const { MailValidationError } = await import("./validation");

// `mock.module` is process-global with no unmock API — hand the real modules
// back so the next file in the same run does not inherit these stubs.
afterAll(() => {
  if (REAL_SERVER) mock.module("server", () => REAL_SERVER);
  mock.module("./mailgun", () => REAL_MAILGUN);
  mockListDomainAccess.mockRestore();
});

const USER = new SignedUser({
  id: "11111111-2222-3333-4444-555555555555",
  username: "admin",
  email: "admin@example.com",
});

const MAIL = new MailDataToSend({
  sender: "admin",
  senderFullName: "",
  to: "recipient@external.com",
  cc: undefined,
  bcc: undefined,
  subject: "Sign in",
  html: "<p>token</p>",
  inReplyTo: undefined,
});

beforeEach(() => {
  mockSaveMail.mockClear();
  mockSendMailgunMail.mockClear();
});

describe("sendMail — persistToSentMailbox", () => {
  it("writes no Sent record when the option is false", async () => {
    const response = await sendMail(USER, MAIL, undefined, {
      persistToSentMailbox: false,
    });

    expect(mockSendMailgunMail).toHaveBeenCalledTimes(1);
    expect(mockSaveMail).toHaveBeenCalledTimes(0);
    expect(response).toEqual({ id: "mailgun-message-id" });
  });

  it("writes the Sent record when the option is omitted", async () => {
    await sendMail(USER, MAIL);

    expect(mockSendMailgunMail).toHaveBeenCalledTimes(1);
    expect(mockSaveMail).toHaveBeenCalledTimes(1);
  });

  it("writes the Sent record when the option is explicitly true", async () => {
    await sendMail(USER, MAIL, undefined, { persistToSentMailbox: true });

    expect(mockSaveMail).toHaveBeenCalledTimes(1);
  });
});

describe("sendMail — the domain it sends from", () => {
  it("sends from the sender's default domain when the mail names none", async () => {
    await sendMail(USER, new MailDataToSend({ ...MAIL }), undefined, { persistToSentMailbox: false });
    expect((mockSendMailgunMail.mock.calls[0] as unknown[])[0]).toBe("example.com");
  });

  it("sends from another domain the sender owns when the mail names it", async () => {
    await sendMail(USER, new MailDataToSend({ ...MAIL, senderDomain: "Example.ORG" }), undefined, {
      persistToSentMailbox: false,
    });
    expect((mockSendMailgunMail.mock.calls[0] as unknown[])[0]).toBe("example.org");
  });

  it("refuses a domain the sender only views, without sending anything", async () => {
    const send = sendMail(USER, new MailDataToSend({ ...MAIL, senderDomain: "example.net" }));
    await expect(send).rejects.toBeInstanceOf(MailValidationError);
    expect(mockSendMailgunMail).not.toHaveBeenCalled();
    expect(mockSaveMail).not.toHaveBeenCalled();
  });

  it("refuses a domain the sender holds no role on", async () => {
    const send = sendMail(USER, new MailDataToSend({ ...MAIL, senderDomain: "elsewhere.test" }));
    await expect(send).rejects.toThrow("You can't send from elsewhere.test.");
    expect(mockSendMailgunMail).not.toHaveBeenCalled();
  });

  it("stores the Sent copy under the address it was sent from", async () => {
    await sendMail(USER, new MailDataToSend({ ...MAIL, senderDomain: "example.org" }));
    const saved = (mockSaveMail.mock.calls[0] as unknown[])[0] as {
      from: { value: { address: string }[] };
    };
    expect(saved.from.value[0].address).toBe("admin@example.org");
  });
});

describe("sendMail — a sending domain Mailgun doesn't have", () => {
  it("tells the sender, and never alarms the operator", async () => {
    const alarm = spyOn(alarmModule, "sendAlarm").mockResolvedValue(undefined as never);
    mockSendMailgunMail.mockRejectedValueOnce(
      Object.assign(new Error("Domain not found: example.org"), { status: 404 })
    );
    try {
      const send = sendMail(USER, new MailDataToSend({ ...MAIL, senderDomain: "example.org" }));
      await expect(send).rejects.toThrow(
        "This server can't send from example.org yet. Ask its operator to add the domain to Mailgun."
      );
      expect(alarm).not.toHaveBeenCalled();
    } finally {
      alarm.mockRestore();
    }
  });

  it("alarms a 404 for a domain under the served zone: that is the operator's setup", async () => {
    const alarm = spyOn(alarmModule, "sendAlarm").mockResolvedValue(undefined as never);
    const previousZone = process.env.EMAIL_DOMAIN;
    process.env.EMAIL_DOMAIN = "example.com";
    mockSendMailgunMail.mockRejectedValueOnce(
      Object.assign(new Error("Domain not found: example.com"), { status: 404 })
    );
    try {
      const send = sendMail(USER, new MailDataToSend({ ...MAIL, senderDomain: "example.com" }));
      await expect(send).rejects.toThrow("Failed to send email. Please try again.");
      expect(alarm).toHaveBeenCalledTimes(1);
    } finally {
      if (previousZone === undefined) delete process.env.EMAIL_DOMAIN;
      else process.env.EMAIL_DOMAIN = previousZone;
      alarm.mockRestore();
    }
  });

  it("alarms a 404 on system mail, which always sends from the zone", async () => {
    const alarm = spyOn(alarmModule, "sendAlarm").mockResolvedValue(undefined as never);
    mockSendMailgunMail.mockRejectedValueOnce(
      Object.assign(new Error("Domain not found"), { status: 404 })
    );
    try {
      const send = sendSystemMail(new MailDataToSend({ ...MAIL, sender: "no-reply" }));
      await expect(send).rejects.toThrow("Failed to send email. Please try again.");
      expect(alarm).toHaveBeenCalledTimes(1);
    } finally {
      alarm.mockRestore();
    }
  });

  it("still alarms any other failure", async () => {
    const alarm = spyOn(alarmModule, "sendAlarm").mockResolvedValue(undefined as never);
    mockSendMailgunMail.mockRejectedValueOnce(Object.assign(new Error("boom"), { status: 500 }));
    try {
      await expect(sendMail(USER, new MailDataToSend({ ...MAIL }))).rejects.toThrow();
      expect(alarm).toHaveBeenCalledTimes(1);
    } finally {
      alarm.mockRestore();
    }
  });
});
