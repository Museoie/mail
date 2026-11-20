import { afterAll, beforeEach, describe, expect, it, spyOn } from "bun:test";
import * as imapRegistry from "../imap/session-registry";
import * as smtpRegistry from "../smtp-registry";
import * as sessionsRepo from "../postgres/repositories/sessions";
import * as usersRepo from "../postgres/repositories/users";
import { handleSsoEvent } from "./events";

// Spied rather than module-mocked: each is a leaf another suite tests for
// real, and a spy is restored when this file is done.
const bump = spyOn(usersRepo, "bumpPasswordVersionByAuthSub");
const deleteForUser = spyOn(sessionsRepo, "deleteSessionsForUser");
const deleteBySid = spyOn(sessionsRepo, "deleteSessionsByAuthSid");
const evictImap = spyOn(imapRegistry, "evictImapSessions");
const evictSmtp = spyOn(smtpRegistry, "evictSmtpConnections");
const spies = [bump, deleteForUser, deleteBySid, evictImap, evictSmtp];

afterAll(() => spies.forEach((spy) => spy.mockRestore()));

beforeEach(() => {
  spies.forEach((spy) => spy.mockReset());
  bump.mockResolvedValue({ userId: "u1", username: "alice" });
  deleteForUser.mockResolvedValue(2);
  deleteBySid.mockResolvedValue(1);
  evictImap.mockReturnValue(1);
  evictSmtp.mockReturnValue(1);
});

const revokedEverywhere = (sub: string) => {
  expect(bump.mock.calls).toEqual([[sub]]);
  expect(deleteForUser.mock.calls).toEqual([["u1"]]);
  expect(evictImap.mock.calls).toEqual([["alice"]]);
  expect(evictSmtp.mock.calls).toEqual([["alice"]]);
  // The fence moves before anything is deleted, so a session row an
  // in-flight request writes back after the delete still fails it.
  expect(bump.mock.invocationCallOrder[0]!).toBeLessThan(
    deleteForUser.mock.invocationCallOrder[0]!
  );
  expect(deleteBySid).not.toHaveBeenCalled();
};

describe("handleSsoEvent", () => {
  it("session.ended deletes the sessions of that provider session only", async () => {
    await handleSsoEvent({ type: "session.ended", jti: "evt_1", sid: "ses_alice" });
    expect(deleteBySid.mock.calls).toEqual([["ses_alice"]]);
    expect(bump).not.toHaveBeenCalled();
    expect(deleteForUser).not.toHaveBeenCalled();
  });

  it("access.changed with no role revokes the linked account everywhere", async () => {
    await handleSsoEvent({ type: "access.changed", jti: "evt_1", sub: "usr_alice", role: null });
    revokedEverywhere("usr_alice");
  });

  it("access.changed to another role changes nothing: any role grants the app", async () => {
    await handleSsoEvent({ type: "access.changed", jti: "evt_1", sub: "usr_alice", role: "admin" });
    spies.forEach((spy) => expect(spy).not.toHaveBeenCalled());
  });

  for (const type of ["user.disabled", "user.deleted", "credential.revoked"]) {
    it(`${type} revokes the linked account everywhere`, async () => {
      await handleSsoEvent({ type, jti: "evt_1", sub: "usr_alice", id: "apw_1" });
      revokedEverywhere("usr_alice");
    });
  }

  it("does nothing more for a subject no account is linked to", async () => {
    bump.mockResolvedValue(null);
    await handleSsoEvent({ type: "user.disabled", jti: "evt_1", sub: "usr_nobody" });
    expect(bump.mock.calls).toEqual([["usr_nobody"]]);
    expect(deleteForUser).not.toHaveBeenCalled();
    expect(evictImap).not.toHaveBeenCalled();
    expect(evictSmtp).not.toHaveBeenCalled();
  });

  it("acknowledges and ignores other events", async () => {
    for (const type of ["invite.accepted", "something.new"]) {
      await handleSsoEvent({ type, jti: "evt_1", sub: "usr_alice", invite: "inv_1" });
    }
    spies.forEach((spy) => expect(spy).not.toHaveBeenCalled());
  });

  it("rejects when a write fails, so the provider delivers the event again", async () => {
    deleteForUser.mockRejectedValue(new Error("connection lost"));
    await expect(
      handleSsoEvent({ type: "user.disabled", jti: "evt_1", sub: "usr_alice" })
    ).rejects.toThrow("connection lost");
  });
});
