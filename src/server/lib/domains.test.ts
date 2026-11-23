import { afterAll, afterEach, beforeEach, describe, expect, it, spyOn } from "bun:test";
import * as domainRepo from "./postgres/repositories/domains";
import * as userRepo from "./postgres/repositories/users";
import { UserModel } from "./postgres/models";
import {
  accountDomain,
  checkVerificationRecord,
  defaultDomain,
  domainOfAddress,
  isInServedZone,
  mailgunDomainFor,
  normalizeDomainName,
  othersCanReadAddress,
  parseScopedLogin,
  resolveDeliveryDomain,
  setTxtResolver,
  verificationRecord,
  type TxtResolver,
} from "./domains";

// One spy per repository function for the whole file, restored once: two
// spies on one export, each restored by its own block, would restore the
// real function under the other block.
const findDomain = spyOn(domainRepo, "findDomain");
const assignDomain = spyOn(domainRepo, "assignDomain");
const readersOfDomain = spyOn(domainRepo, "readersOfDomain");
const findUserByUsername = spyOn(userRepo, "findUserByUsername");

const originalDomain = process.env.EMAIL_DOMAIN;
beforeEach(() => {
  process.env.EMAIL_DOMAIN = "Example.com";
});
afterAll(() => {
  findDomain.mockRestore();
  assignDomain.mockRestore();
  readersOfDomain.mockRestore();
  findUserByUsername.mockRestore();
  if (originalDomain === undefined) delete process.env.EMAIL_DOMAIN;
  else process.env.EMAIL_DOMAIN = originalDomain;
});

describe("normalizeDomainName", () => {
  it("lower-cases and trims a name, dropping a trailing dot", () => {
    expect(normalizeDomainName("  Example.ORG. ")).toBe("example.org");
    expect(normalizeDomainName("xn--bcher-kva.example")).toBe("xn--bcher-kva.example");
  });

  it("refuses what is not a domain name", () => {
    for (const input of [
      undefined,
      42,
      "",
      "localhost",
      "-bad.example",
      "bad-.example",
      "under_score.example",
      "a..example",
      "192.168.0.1",
      "white space.example",
      "quote'.example",
      `${"a".repeat(64)}.example`,
      `${"a.".repeat(127)}example`,
    ]) {
      expect(normalizeDomainName(input)).toBeNull();
    }
  });
});

describe("the served zone", () => {
  it("covers the zone itself and every name under it, ignoring case", () => {
    expect(isInServedZone("example.com")).toBe(true);
    expect(isInServedZone("Bob.EXAMPLE.com")).toBe(true);
    expect(isInServedZone("notexample.com")).toBe(false);
    expect(isInServedZone("example.com.attacker.test")).toBe(false);
  });

  it("gives an account its own name under the zone", () => {
    expect(accountDomain("Bob")).toBe("bob.example.com");
  });
});

describe("domainOfAddress", () => {
  it("is the lower-cased part after the last @", () => {
    expect(domainOfAddress("Info@Example.ORG")).toBe("example.org");
    expect(domainOfAddress("\"a@b\"@example.org")).toBe("example.org");
    expect(domainOfAddress("no-at-sign")).toBe("");
  });
});

describe("parseScopedLogin", () => {
  it("splits a login into the person and the domain it is scoped to", () => {
    expect(parseScopedLogin("bob")).toEqual({ username: "bob" });
    expect(parseScopedLogin("bob@Example.ORG")).toEqual({ username: "bob", domain: "example.org" });
  });
});

describe("defaultDomain", () => {
  it("prefers the served zone, then the account's own name, then the first domain", () => {
    expect(defaultDomain(["admin.example.com", "example.com"], "admin")).toBe("example.com");
    expect(defaultDomain(["example.org", "bob.example.com"], "bob")).toBe("bob.example.com");
    expect(defaultDomain(["example.org", "example.net"], "bob")).toBe("example.org");
    expect(defaultDomain([], "bob")).toBeUndefined();
  });
});

describe("mailgunDomainFor", () => {
  it("sends a name under the zone through the zone, any other through itself", () => {
    expect(mailgunDomainFor("bob.example.com")).toBe("Example.com");
    expect(mailgunDomainFor("example.org")).toBe("example.org");
  });
});

describe("checkVerificationRecord", () => {
  let previous: TxtResolver;
  const resolveWith = (resolver: TxtResolver) => {
    previous = setTxtResolver(resolver);
  };
  afterEach(() => {
    if (previous) setTxtResolver(previous);
  });

  it("asks for the TXT record under the verification label", async () => {
    const asked: string[] = [];
    resolveWith(async (host) => {
      asked.push(host);
      return [];
    });
    await checkVerificationRecord("example.org", "tok");
    expect(asked).toEqual(["_mail-verification.example.org"]);
    expect(verificationRecord("example.org", "tok")).toEqual({
      type: "TXT",
      host: "_mail-verification.example.org",
      value: "mail-verification=tok",
    });
  });

  it("finds the record among others, joining a record split into chunks", async () => {
    resolveWith(async () => [["v=spf1 -all"], ["mail-verif", "ication=tok"]]);
    expect(await checkVerificationRecord("example.org", "tok")).toBe("found");
  });

  it("is missing when no record carries this claim's token", async () => {
    resolveWith(async () => [["mail-verification=someone-elses"]]);
    expect(await checkVerificationRecord("example.org", "tok")).toBe("missing");
  });

  it("tells a name without the record from DNS that could not be asked", async () => {
    resolveWith(async () => {
      throw Object.assign(new Error("nx"), { code: "ENOTFOUND" });
    });
    expect(await checkVerificationRecord("example.org", "tok")).toBe("missing");
    resolveWith(async () => {
      throw Object.assign(new Error("timeout"), { code: "ETIMEOUT" });
    });
    expect(await checkVerificationRecord("example.org", "tok")).toBe("unavailable");
  });
});

describe("resolveDeliveryDomain", () => {
  const account = (columns: Record<string, unknown>) =>
    new UserModel({
      user_id: "u-bob",
      username: "bob",
      password: "$2b$10$x",
      password_version: 1,
      email: null,
      expiry: null,
      token: null,
      updated: null,
      is_deleted: false,
      imap_uid_validity: null,
      auth_sub: null,
      ...columns,
    });

  beforeEach(() => {
    findDomain.mockReset();
    findDomain.mockResolvedValue(null);
    assignDomain.mockReset();
    assignDomain.mockResolvedValue(true);
    findUserByUsername.mockReset();
    findUserByUsername.mockResolvedValue(null);
  });

  it("delivers by the domain row, whatever the domain", async () => {
    findDomain.mockResolvedValue({ domainId: "d", name: "example.org", ownerId: "u-1" });
    expect(await resolveDeliveryDomain("Info@Example.ORG")).toEqual({
      domain: "example.org",
      ownerId: "u-1",
    });
    expect(findDomain).toHaveBeenCalledWith("example.org");
  });

  it("gives an account under the zone its row when it has none yet", async () => {
    findUserByUsername.mockResolvedValue(account({}));
    expect(await resolveDeliveryDomain("x@bob.example.com")).toEqual({
      domain: "bob.example.com",
      ownerId: "u-bob",
    });
    expect(assignDomain).toHaveBeenCalledWith("u-bob", "bob.example.com");
  });

  it("delivers nothing for the bare zone without a row, nor for a foreign domain", async () => {
    expect(await resolveDeliveryDomain("x@example.com")).toBeNull();
    expect(await resolveDeliveryDomain("x@elsewhere.test")).toBeNull();
    expect(findUserByUsername).not.toHaveBeenCalled();
  });

  it("delivers nothing to a soft-deleted owner, and keeps its name taken", async () => {
    findDomain.mockResolvedValue({ domainId: "d", name: "bob.example.com", ownerId: "u-bob", ownerDeleted: true });
    expect(await resolveDeliveryDomain("x@bob.example.com")).toBeNull();
    expect(findUserByUsername).not.toHaveBeenCalled();
    expect(assignDomain).not.toHaveBeenCalled();
  });

  it("gives a sign-up still setting its password its row, as the backfill does", async () => {
    findUserByUsername.mockResolvedValue(account({ password: null }));
    expect(await resolveDeliveryDomain("x@bob.example.com")).toEqual({ domain: "bob.example.com", ownerId: "u-bob" });
  });

  it("delivers nothing to a deleted or missing account under the zone", async () => {
    for (const row of [account({ is_deleted: true }), null]) {
      findUserByUsername.mockResolvedValue(row);
      expect(await resolveDeliveryDomain("x@bob.example.com")).toBeNull();
    }
    expect(assignDomain).not.toHaveBeenCalled();
  });

  it("takes the row another delivery assigned first", async () => {
    findUserByUsername.mockResolvedValue(account({}));
    assignDomain.mockResolvedValue(false);
    findDomain
      .mockResolvedValueOnce(null)
      .mockResolvedValueOnce({ domainId: "d", name: "bob.example.com", ownerId: "u-other" });
    expect(await resolveDeliveryDomain("x@bob.example.com")).toEqual({
      domain: "bob.example.com",
      ownerId: "u-other",
    });
  });
});

describe("othersCanReadAddress", () => {
  beforeEach(() => {
    findDomain.mockReset();
    findDomain.mockResolvedValue({ domainId: "d", name: "example.org", ownerId: "u-owner" });
    readersOfDomain.mockReset();
  });

  it("is false when the account is the only one reading its address", async () => {
    readersOfDomain.mockResolvedValue(["u-owner"]);
    expect(await othersCanReadAddress("me@example.org", "u-owner")).toBe(false);
  });

  it("is true when the domain's owner is someone else", async () => {
    readersOfDomain.mockResolvedValue(["u-owner"]);
    expect(await othersCanReadAddress("me@example.org", "u-account")).toBe(true);
  });

  it("is true when someone views the account's own domain", async () => {
    readersOfDomain.mockResolvedValue(["u-owner", "u-viewer"]);
    expect(await othersCanReadAddress("me@example.org", "u-owner")).toBe(true);
  });

  it("is true for any reader when there is no account yet", async () => {
    readersOfDomain.mockResolvedValue(["u-owner"]);
    expect(await othersCanReadAddress("new@example.org")).toBe(true);
  });

  it("is false for an address this server delivers nowhere", async () => {
    findDomain.mockResolvedValue(null);
    expect(await othersCanReadAddress("someone@elsewhere.test")).toBe(false);
    expect(readersOfDomain).not.toHaveBeenCalled();
  });
});
