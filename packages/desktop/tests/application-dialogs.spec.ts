import { afterEach, expect, it } from "vitest";
import {
  applicationDialogs,
  requestConfirmation,
  requestPrompt,
} from "../src/lib/application-dialogs";

afterEach(() => applicationDialogs.cancel());

it("keeps a stale close from resolving the next queued dialog", async () => {
  const firstResult = requestConfirmation("Delete?");
  const first = applicationDialogs.current()!;
  const secondResult = requestPrompt("Name", "example");
  applicationDialogs.complete(first, true);
  applicationDialogs.complete(first, false);
  const second = applicationDialogs.current()!;
  expect(second.message).toBe("Name");
  applicationDialogs.complete(second, "renamed");
  expect(await Promise.all([firstResult, secondResult])).toEqual([
    true,
    "renamed",
  ]);
});

it("cancels pending requests when their UI owner unmounts", async () => {
  const results = [requestPrompt("Name"), requestConfirmation("Delete?")];
  applicationDialogs.cancel();
  expect(await Promise.all(results)).toEqual([null, false]);
});

it("publishes queue transitions and releases subscribers", () => {
  let updates = 0;
  const unsubscribe = applicationDialogs.subscribe(() => updates++);
  void requestPrompt("Name");
  applicationDialogs.complete(applicationDialogs.current()!, "");
  unsubscribe();
  void requestConfirmation("Delete?");
  expect(updates).toBe(2);
});
