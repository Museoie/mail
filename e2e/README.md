# Black-box suite

Tests that drive a running Mail server from outside — over HTTP, SMTP and
IMAP — and assert on what it answers and what it leaves in the database.
The same files run against the Bun server (`src/server`) and the Rust server
(`src/server-rs`); a behaviour is only "done" when both pass.

```bash
bun run test:e2e            # both servers, one after the other
bun run test:e2e:bun        # the Bun server only
bun run test:e2e:rust       # builds and tests the Rust server
bun scripts/e2e.ts --server rust --debug users   # debug build, files matching "users"
```

These files end in `.e2e.ts` so the unit-test run (`bun run test`) never
picks them up.

## What it needs

- **PostgreSQL** it may create and drop databases on. Defaults to
  `postgres:postgres@127.0.0.1:5432`; override with
  `MAIL_E2E_POSTGRES_HOST`, `_PORT`, `_USER`, `_PASSWORD`.
- **`openssl`** on the path, for the TLS tests' throwaway certificate.
- **A Rust toolchain** for the Rust run.

Nothing else: Mailgun, the Discord alarm webhook and the single sign-on
provider are stubs the harness starts, and every server gets its own empty
database, free ports and working directory.

## Writing a test

```ts
import { afterAll, beforeAll, expect, test } from "bun:test";
import { ADMIN_PASSWORD, adminUser, addr, insertMail, startServer, type TestServer } from "../harness";

let server: TestServer;
beforeAll(async () => { server = await startServer(); }, 90_000);
afterAll(() => server?.stop());

test("an unread mail is counted", async () => {
  const admin = await adminUser(server);
  await insertMail(server, admin.id, { to: [addr("hello@mail.test")] });

  const client = server.client();
  await client.login("admin", ADMIN_PASSWORD);
  const { body } = await client.get("/api/mails/accounts");
  expect(body.body.received[0].unread_doc_count).toBe(1);
});
```

- `startServer(options)` boots the server under test, then seeds `admin`
  (password `ADMIN_PASSWORD`) owning the served zone, as an operator does
  after installing; `seedAdmin: false` leaves the database empty.
  `createUser` inserts an account with its own `<username>.mail.test`, and
  `giveDomains` makes an account the owner of more. `production: true`
  gives the production posture, where a client names its own address with
  `X-Forwarded-For`, so each `server.client()` has its own rate-limit
  buckets. `tls: true` opens the TLS listeners. `env` sets anything else.
- `server.db.query(sql, values)` reads and seeds the database directly.
- `server.mailgun.requests` and `server.alarms.alarms` are what the server
  sent out.
- `deliverMail` / `rfc822` (in `harness/deliver.ts`) put mail into a mailbox
  the way a relay would. Scripted protocol clients live in `harness/smtp.ts`
  and `harness/imap.ts`, and a fake OpenID Connect provider in
  `harness/issuer.ts`; import them by path.
- Never branch on which server is under test. If the two differ, one of them
  is wrong.

Set `MAIL_E2E_LOGS=1` to stream the server's output while a test runs, or
print `server.logs()`. `bun run typecheck:e2e` type-checks the suite.
