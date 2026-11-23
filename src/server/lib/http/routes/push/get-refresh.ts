import { push } from "server";
import { Route } from "../route";

export type RefreshGetResponse = undefined;

// A read-only session carries admin's id, so it must not refresh admin's subscriptions.
export const getRefreshRoute = new Route<RefreshGetResponse>(
  "GET",
  "/refresh/:id",
  async (req) => {
    const user = req.session.user;
    const result =
      !!user &&
      !user.isReadOnly &&
      (await push.refreshSubscription(req.params.id, user.id));
    if (result) {
      return { status: "success" };
    } else {
      return { status: "failed", message: "No subscription found" };
    }
  }
);
