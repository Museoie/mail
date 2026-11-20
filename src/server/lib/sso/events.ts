import type { AuthEvent } from "../auth-client";
import { evictImapSessions } from "../imap/session-registry";
import { logger } from "../logger";
import {
  deleteSessionsByAuthSid,
  deleteSessionsForUser,
} from "../postgres/repositories/sessions";
import { bumpPasswordVersionByAuthSub } from "../postgres/repositories/users";
import { evictSmtpConnections } from "../smtp-registry";

/**
 * Takes away everything the linked account signed in with. The credential
 * generation moves first: every process checks it, so HTTP sessions and the
 * IMAP and SMTP connections other copies of the server hold fail their next
 * request or command. This copy then deletes the sessions and closes its own
 * connections at once.
 */
const revokeLinkedAccount = async (sub: string, cause: string): Promise<void> => {
  const account = await bumpPasswordVersionByAuthSub(sub);
  if (!account) return;
  const deletedSessions = await deleteSessionsForUser(account.userId);
  const imapConnections = evictImapSessions(account.username);
  const smtpConnections = evictSmtpConnections(account.username);
  logger.info("Revoked a linked account's sessions and connections", {
    component: "sso",
    cause,
    userId: account.userId,
    deletedSessions,
    imapConnections,
    smtpConnections,
  });
};

/**
 * Applies one provider event. Delivery is at least once, and applying an
 * event again only ends sessions and connections again, so no record of
 * handled events is kept. A fault rejects, and the provider retries.
 */
export const handleSsoEvent = async (event: AuthEvent): Promise<void> => {
  switch (event.type) {
    case "session.ended": {
      if (!event.sid) return;
      const deletedSessions = await deleteSessionsByAuthSid(event.sid);
      logger.info("Ended the sessions of a provider session", {
        component: "sso",
        deletedSessions,
      });
      return;
    }
    case "access.changed":
      // Any role grants the whole app, so only its removal changes anything.
      if (event.sub && event.role === null) await revokeLinkedAccount(event.sub, event.type);
      return;
    case "user.disabled":
    case "user.deleted":
    case "credential.revoked":
      if (event.sub) await revokeLinkedAccount(event.sub, event.type);
      return;
    default:
      return;
  }
};
