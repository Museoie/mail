/**
 * `saveMailHandler` answers how many mailboxes it wrote, and the SMTP handler
 * turns a zero into a transient refusal rather than a `250`. The property is
 * "the count is writes, not recipients" — a count of recipients answers `250`
 * for a message nothing was stored for, which is the acceptance the envelope
 * gate exists to prevent.
 *
 * Which domain an address is delivered into is injected (`resolveDelivery`),
 * and an owner with no account row returns from `saveIncomingMail` at its own
 * `if (!user)` guard before anything touches the pool, so stubbing `getUser`
 * is the whole harness.
 */
import { describe, it, expect, mock, afterAll, beforeEach } from "bun:test";
import type { IncomingMail } from "common";
import type { DeliveryDomain } from "../domains";

const REAL_SERVER = (globalThis as Record<string, unknown>).__REAL_SERVER as Record<
  string,
  unknown
>;

const mockGetUser = mock(async (_input: unknown) => undefined);

mock.module("server", () => ({
  ...REAL_SERVER,
  getUser: mockGetUser,
}));

const { saveMailHandler } = await import("./receive");

// `mock.module` is process-global with no unmock API — hand the real module
// back so the next file in the same run does not inherit this stub.
afterAll(() => {
  if (REAL_SERVER) mock.module("server", () => REAL_SERVER);
});

const makeMail = (addresses: string[]): IncomingMail =>
  ({ envelopeTo: addresses.map((address) => ({ address })) } as unknown as IncomingMail);

/** Delivers the addresses on `domains` into the named owners. */
const deliveringTo = (domains: Record<string, string>) =>
  mock(async (address: string): Promise<DeliveryDomain | null> => {
    const domain = address.split("@").pop()!.toLowerCase();
    return domains[domain] ? { domain, ownerId: domains[domain] } : null;
  });

describe("saveMailHandler stored count", () => {
  beforeEach(() => mockGetUser.mockClear());

  it("answers 0 when no recipient is delivered anywhere, without looking an owner up", async () => {
    const resolveDelivery = deliveringTo({});

    const stored = await saveMailHandler(null, makeMail(["someone@other.test"]), {
      resolveDelivery,
    });

    expect(stored).toBe(0);
    expect(resolveDelivery).toHaveBeenCalledWith("someone@other.test");
    expect(mockGetUser).not.toHaveBeenCalled();
  });

  it("answers 0 when every owner it resolves to has no account", async () => {
    const stored = await saveMailHandler(
      null,
      makeMail(["x@one.test", "y@two.test"]),
      { resolveDelivery: deliveringTo({ "one.test": "owner-1", "two.test": "owner-2" }) }
    );

    expect(stored).toBe(0);
    expect(mockGetUser).toHaveBeenCalledTimes(2);
    expect(mockGetUser).toHaveBeenCalledWith({ id: "owner-1" });
    expect(mockGetUser).toHaveBeenCalledWith({ id: "owner-2" });
  });

  it("looks each owner up once however many of their domains the message names", async () => {
    await saveMailHandler(
      null,
      makeMail(["x@one.test", "Y@ONE.test", "z@two.test", "x@one.test"]),
      { resolveDelivery: deliveringTo({ "one.test": "owner-1", "two.test": "owner-1" }) }
    );

    expect(mockGetUser).toHaveBeenCalledTimes(1);
    expect(mockGetUser).toHaveBeenCalledWith({ id: "owner-1" });
  });
});
