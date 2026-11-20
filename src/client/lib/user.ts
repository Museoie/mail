import { LoginGetResponse } from "server";
import { call } from "./call";
import { getLocalStorageItem, setLocalStorageItem } from "./cache";
import { idbClearQueries } from "./idbStore";

export type SignInMode = LoginGetResponse["sso"];

let signInMode: SignInMode = { enabled: false, localLogin: true };

/**
 * How this server signs people in, as the last `callUser` answer said. The
 * app mounts only after the first answer, so the sign-in page renders with it.
 */
export const getSignInMode = (): SignInMode => signInMode;

export const callUser = () =>
  call.get<LoginGetResponse>("/api/users/login").then(async (r) => {
    if (r.body?.sso) signInMode = r.body.sso;
    const app = r.body?.app;
    const version = app?.version;
    if (version) {
      const appInfo = getLocalStorageItem("app");
      const theVersionThatIUsedToKnow = appInfo?.version;
      if (theVersionThatIUsedToKnow !== version) {
        localStorage.clear();
        // A new deploy can change cached payload shapes; drop the IndexedDB
        // query cache too so hydration can't seed a stale-schema response.
        // Awaited so the purge completes before mountApp's hydrate reads it.
        await idbClearQueries();
        setLocalStorageItem("app", app);
      }
    }
    return r.body?.user;
  });

/**
 * Where the browser goes to sign in with single sign-on, coming back to
 * `returnTo` afterwards. A full-page navigation, not a fetch: the provider's
 * sign-in page has to load.
 */
export const ssoLoginUrl = (returnTo = "/"): string =>
  `/api/auth/login?returnTo=${encodeURIComponent(returnTo)}`;
