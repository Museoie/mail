/**
 * `initializeDomains`, at every boot: gives each account that has none its
 * own domain under the served zone, and warns when no account owns the zone
 * itself. The repositories are spied, so the assertions are on what the step
 * asks of them; the backfill statement itself is probed against Postgres in
 * `repositories/domains.test.ts`.
 */
import { afterAll, beforeEach, describe, expect, it, spyOn } from "bun:test";
import * as domainRepo from "./repositories/domains";
import { logger } from "../logger";
import { initializeDomains } from "./initialize";

const backfill = spyOn(domainRepo, "backfillAccountDomains");
const findDomain = spyOn(domainRepo, "findDomain");
const warn = spyOn(logger, "warn");

const originalDomain = process.env.EMAIL_DOMAIN;

afterAll(() => {
  backfill.mockRestore();
  findDomain.mockRestore();
  warn.mockRestore();
  if (originalDomain === undefined) delete process.env.EMAIL_DOMAIN;
  else process.env.EMAIL_DOMAIN = originalDomain;
});

beforeEach(() => {
  process.env.EMAIL_DOMAIN = "Example.com";
  backfill.mockReset();
  backfill.mockResolvedValue(0);
  findDomain.mockReset();
  findDomain.mockResolvedValue({ domainId: "d", name: "example.com", ownerId: "u-1" });
  warn.mockReset();
  warn.mockImplementation(() => undefined);
});

const warnings = () => warn.mock.calls.map((call) => String(call[0])).join("\n");

describe("initializeDomains", () => {
  it("backfills the accounts' own domains under the lower-cased zone", async () => {
    await initializeDomains();
    expect(backfill).toHaveBeenCalledWith("example.com");
    expect(warnings()).toBe("");
  });

  it("warns, naming the command, when no account owns the served zone", async () => {
    findDomain.mockResolvedValue(null);
    await initializeDomains();
    expect(findDomain).toHaveBeenCalledWith("example.com");
    expect(warnings()).toContain("No account owns example.com");
    expect(warnings()).toContain("bun run user add-domain <username> example.com");
  });

  it("lets a failed backfill stop the boot rather than start without rows", async () => {
    backfill.mockRejectedValue(new Error("connection lost"));
    await expect(initializeDomains()).rejects.toThrow("connection lost");
  });
});
