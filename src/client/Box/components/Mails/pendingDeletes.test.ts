import { describe, it, expect, afterEach } from "bun:test";
import {
  cancelDelete,
  flushDeletes,
  pendingDeleteCount,
  scheduleDelete
} from "./pendingDeletes";

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

afterEach(() => flushDeletes());

describe("pending deletes", () => {
  it("runs the delete once the undo window passes", async () => {
    const runs: string[] = [];
    scheduleDelete("a", () => runs.push("a"), 10);
    expect(runs).toEqual([]);
    await wait(30);
    expect(runs).toEqual(["a"]);
    expect(pendingDeleteCount()).toBe(0);
  });

  it("never runs a cancelled delete", async () => {
    const runs: string[] = [];
    scheduleDelete("a", () => runs.push("a"), 10);
    expect(cancelDelete("a")).toBe(true);
    await wait(30);
    expect(runs).toEqual([]);
    expect(cancelDelete("a")).toBe(false);
  });

  it("flush runs every waiting delete immediately, once", async () => {
    const runs: string[] = [];
    scheduleDelete("a", () => runs.push("a"), 1000);
    scheduleDelete("b", () => runs.push("b"), 1000);
    flushDeletes();
    expect(runs.sort()).toEqual(["a", "b"]);
    await wait(10);
    flushDeletes();
    expect(runs.length).toBe(2);
  });

  it("rescheduling the same mail replaces the earlier delete", async () => {
    const runs: string[] = [];
    scheduleDelete("a", () => runs.push("first"), 10);
    scheduleDelete("a", () => runs.push("second"), 10);
    await wait(30);
    expect(runs).toEqual(["second"]);
  });
});
