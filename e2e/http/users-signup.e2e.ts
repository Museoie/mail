import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import {
  ADMIN_PASSWORD,
  MAILGUN_KEY,
  adminUser,
  createUser,
  type MailgunRequest,
  type TestServer,
  type TestUser,
} from "../harness";
import { ImapClient } from "../harness/imap";
import { startServer } from "./core-helpers";
import {
  INTERNAL_ERROR,
  INVALID_BODY,
  INVALID_CREDENTIALS,
  NON_STRINGS,
  isAbout,
  onlySetCookie,
  expectSessionCookie,
  sessionIdOf,
  sessionIdOfValue,
  sessionRow,
  userRow,
  userRowByEmail,
} from "./core-helpers";

const HOUR = 3_600_000;
const INVALID_EMAIL = { status: "failed", message: "Signup failed because email is invalid." };
const SUCCESS = { status: "success" };

/** Production posture, so every client has its own address and token quota. */
let server: TestServer;
let admin: { id: string; email: string };

beforeAll(async () => {
  server = await startServer({ production: true });
  admin = await adminUser(server);
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const requestToken = (email: unknown) => server.client().post("/api/users/token", { email });
const userCount = async () => (await server.db.query("SELECT COUNT(*)::int AS count FROM users"))[0]!.count as number;
const mailsTo = (email: string): MailgunRequest[] =>
  server.mailgun.requests.filter((request) => request.fields.to?.includes(email));

/** The exact HTML of the sign-up / reset mail for `href`; two of its lines end in spaces. */
const authenticationHtml = (href: string) =>
  [
    "",
    '<h2 style="text-align: center;">Thanks for signing up with Mail!</h2>',
    '<p style="text-align: center;">',
    "  You have requested membership confirmation email for Mail.",
    "  <br/>",
    "  Please click the button below to complete setting user information.",
    "</p>",
    "<a",
    `  href="${href}"`,
    '  target="mail-confirm"',
    '  style="display: flex; align-items: center; justify-content: center; margin: 2rem auto; padding: 5px; background-color: #3291FF; color: white; border: none; height: 40px; width: 400px; font-size: 20px;"',
    ">Confirm Sign Up</a>",
    '<p style="text-align: center;">',
    "  * This email expires in 1 hour.",
    "  <br/>" + "  ",
    "  * You should not share this email with other people.",
    "  <br/>" + "  ",
    "  * Ignore this email if you have not requested it.",
    "</p>",
    "",
  ].join("\n");

const AUTHENTICATION_TEXT =
  "THANKS FOR SIGNING UP WITH MAIL!\n" +
  "You have requested membership confirmation email for Mail.\n" +
  "Please click the button below to complete setting user information.\n" +
  "Confirm Sign Up\n" +
  "* This email expires in 1 hour.\n" +
  "* You should not share this email with other people.\n" +
  "* Ignore this email if you have not requested it.";

describe("POST /api/users/token — what it refuses", () => {
  test("a body that is an array", async () => {
    for (const body of [[], [{ email: "someone@elsewhere.example" }], ["someone@elsewhere.example"]]) {
      const { status, body: answer } = await server.client().post("/api/users/token", body);
      expect(status).toBe(200);
      expect(answer).toEqual(INVALID_BODY);
    }
  });

  test.each(NON_STRINGS)("an email that is %s", async (_label, email) => {
    const { status, body } = await requestToken(email);
    expect(status).toBe(200);
    expect(body).toEqual(INVALID_EMAIL);
  });

  test("a missing email, or no body at all", async () => {
    expect((await server.client().post("/api/users/token", {})).body).toEqual(INVALID_EMAIL);
    expect((await server.client().request("POST", "/api/users/token")).body).toEqual(INVALID_EMAIL);
    const formEncoded = await server.client().request("POST", "/api/users/token", {
      body: new URLSearchParams({ email: "someone@elsewhere.example" }),
    });
    expect(formEncoded.body).toEqual(INVALID_EMAIL);
  });

  test.each([
    ["empty", ""],
    ["no @", "someone.elsewhere.example"],
    ["two @", "some@one@elsewhere.example"],
    ["only an @", "@"],
    ["an empty local part", "@elsewhere.example"],
    ["an empty domain", "someone@"],
    ["a domain without a dot", "someone@localhost"],
    ["a space in the local part", "some one@elsewhere.example"],
    ["a space in the domain", "someone@else where.example"],
    ["a leading space", " someone@elsewhere.example"],
    ["a trailing space", "someone@elsewhere.example "],
    ["a trailing newline", "someone@elsewhere.example\n"],
    ["a quote in the local part", 'some"one@elsewhere.example'],
    ["an apostrophe in the local part", "o'neil@elsewhere.example"],
    ["a slash in the local part", "some/one@elsewhere.example"],
    ["an exclamation mark in the local part", "some!one@elsewhere.example"],
    ["an angle-bracketed address", "<someone@elsewhere.example>"],
    ["a display name", "Someone <someone@elsewhere.example>"],
    ["a non-ASCII local part", "sömeone@elsewhere.example"],
    ["a non-ASCII domain", "someone@elsewhère.example"],
    ["an underscore in the domain", "someone@else_where.example"],
    ["a percent sign in the domain", "someone@else%where.example"],
    ["a plus in the domain", "someone@else+where.example"],
    ["a bracketed address literal", "someone@[192.0.2.1]"],
    ["a port in the domain", "someone@elsewhere.example:25"],
    ["a comma-separated pair", "a@elsewhere.example,b@elsewhere.example"],
    ["a NUL byte", "some\u0000one@elsewhere.example"],
  ])("an invalid email: %s", async (_label, email) => {
    const users = await userCount();
    const { status, body } = await requestToken(email);
    expect(status).toBe(200);
    expect(body).toEqual(INVALID_EMAIL);
    expect(await userCount()).toBe(users);
    expect(mailsTo(email)).toEqual([]);
  });
});

describe("POST /api/users/token — addresses that get a success and no mail", () => {
  const expectSilentSuccess = async (email: string) => {
    const users = await userCount();
    const sent = server.mailgun.requests.length;
    const { status, text, setCookies } = await requestToken(email);
    expect({ email, status, text }).toEqual({ email, status: 200, text: '{"status":"success"}' });
    expect(setCookies).toEqual([]);
    expect(await userCount()).toBe(users);
    expect(server.mailgun.requests.length).toBe(sent);
  };

  test("an account's address on a domain someone else reads", async () => {
    // admin owns mail.test; a viewer of it would read the token mailed there.
    const viewer = await createUser(server, "watcher");
    await server.db.query(
      `INSERT INTO domain_roles (domain_id, user_id, role)
       SELECT domain_id, $1, 'viewer' FROM domains WHERE name = 'mail.test'`,
      [viewer.id]
    );
    try {
      await expectSilentSuccess(admin.email);
      expect(await userRow(server, "admin")).toMatchObject({ token: null, expiry: null });
    } finally {
      await server.db.query("DELETE FROM domain_roles WHERE user_id = $1", [viewer.id]);
    }
  });

  test.each([
    ["anything@mail.test"],
    ["ANYTHING@MAIL.TEST"],
    ["x@admin.mail.test"],
    ["x@Admin.Mail.Test"],
    ["a.b+c%d_e-f@mail.test"],
    ["0@mail.test"],
    ["-@mail.test"],
    [".@mail.test"],
    ["%@mail.test"],
    [`${"l".repeat(300)}@mail.test`],
  ])("an address delivered to admin's mailbox: %s", async (email) => {
    await expectSilentSuccess(email);
  });

  // Who reads an address is decided by the domain rows, not by how its name reads.
  test("a domain that only starts with admin.<domain> is an outside address, and signs up", async () => {
    for (const email of ["x@admin.mail.test.evil.example", "x@admin.mail.testing.example"]) {
      const users = await userCount();
      expect((await requestToken(email)).body).toEqual(SUCCESS);
      expect(await userCount()).toBe(users + 1);
      expect(mailsTo(email)).toHaveLength(1);
    }
  });
});

describe("POST /api/users/token — admin is an account like any other", () => {
  // NOTE: the link is never sent. A send whose every recipient is on the
  // served zone itself skips Mailgun, and a system mail is stored nowhere, so
  // admin@<EMAIL_DOMAIN> gets a token it cannot read. An address elsewhere
  // (below) works.
  test("its own address gets a reset token while nobody else reads its domain", async () => {
    const sent = server.mailgun.requests.length;
    expect((await requestToken(admin.email)).body).toEqual(SUCCESS);
    try {
      const row = (await userRow(server, "admin"))!;
      expect(row.token).toMatch(/^[0-9a-f]{64}$/);
      expect(server.mailgun.requests.length).toBe(sent);
    } finally {
      await server.db.query("UPDATE users SET token = NULL, expiry = NULL WHERE username = 'admin'");
    }
  });

  test("so does an address outside the served domain", async () => {
    await server.db.query("UPDATE users SET email = 'owner@elsewhere.example' WHERE username = 'admin'");
    try {
      expect((await requestToken("owner@elsewhere.example")).body).toEqual(SUCCESS);
      expect((await userRow(server, "admin"))!.token).toMatch(/^[0-9a-f]{64}$/);
      expect(mailsTo("owner@elsewhere.example")).toHaveLength(1);
    } finally {
      await server.db.query("UPDATE users SET email = $1, token = NULL, expiry = NULL WHERE username = 'admin'", [admin.email]);
    }
  });
});

describe("POST /api/users/token — a registered user's address (password reset)", () => {
  let alice: TestUser;
  beforeAll(async () => {
    alice = await createUser(server, "alice");
  });

  test("writes a one-hour token onto the existing row and mails a link that names the user", async () => {
    const before = (await userRow(server, "alice"))!;
    const users = await userCount();
    const { status, text, setCookies } = await requestToken(alice.email);
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success"}');
    expect(setCookies).toEqual([]);

    const row = (await userRow(server, "alice"))!;
    expect(await userCount()).toBe(users);
    expect(row.token).toMatch(/^[0-9a-f]{64}$/);
    expect(isAbout(row.expiry!, HOUR)).toBe(true);
    expect(row).toMatchObject({
      user_id: alice.id,
      password: before.password,
      password_version: 1,
      email: alice.email,
      is_deleted: false,
    });
    expect(row.updated.getTime()).toBeGreaterThan(before.updated.getTime());

    const [mail, ...others] = mailsTo(alice.email);
    expect(others).toEqual([]);
    expect(mail).toEqual({
      method: "POST",
      path: "/v3/mail.test/messages",
      domain: "mail.test",
      authorization: `Basic ${Buffer.from(`api:${MAILGUN_KEY}`).toString("base64")}`,
      contentType: expect.stringMatching(/^multipart\/form-data; boundary=\S+$/),
      fieldOrder: ["from", "to", "subject", "html", "text"],
      fields: {
        from: ["Mail <no-reply@mail.test>"],
        to: [alice.email],
        subject: ["Please set your password for Mail"],
        html: [authenticationHtml(`https://app.mail.test/set-info/${alice.email}?t=${row.token}&u=alice`)],
        text: [AUTHENTICATION_TEXT],
      },
      attachments: [],
    });
  });

  test("a second request replaces the token", async () => {
    await requestToken(alice.email);
    const first = (await userRow(server, "alice"))!.token;
    const { body } = await requestToken(alice.email);
    expect(body).toEqual(SUCCESS);
    const second = (await userRow(server, "alice"))!.token;
    expect(second).toMatch(/^[0-9a-f]{64}$/);
    expect(second).not.toBe(first);
    const latest = mailsTo(alice.email).at(-1)!;
    expect(latest.fields.html![0]).toContain(`?t=${second}&u=alice`);
  });

  test("leaves the user's sessions and password alone", async () => {
    const client = server.client();
    await client.login(alice.username, alice.password);
    await requestToken(alice.email);
    expect((await client.get("/api/users/login")).body.body.user.username).toBe("alice");
    const again = await server.client().post("/api/users/login", { username: "alice", password: alice.password });
    expect(again.body.status).toBe("success");
  });

  test("the address is matched exactly: another letter case is a new sign-up", async () => {
    const email = alice.email.toUpperCase();
    const { body } = await requestToken(email);
    expect(body).toEqual(SUCCESS);
    const placeholder = (await userRowByEmail(server, email))!;
    expect(placeholder.username).toMatch(/^user_[0-9a-f]{8}$/);
    expect(placeholder.user_id).not.toBe(alice.id);
    expect(mailsTo(email).at(-1)!.fields.html![0]).not.toContain("&u=");
  });
});

describe("POST /api/users/token — a new address (sign-up)", () => {
  test("creates a placeholder row named after its id and mails a link without a username", async () => {
    const email = "newcomer@elsewhere.example";
    const users = await userCount();
    const { status, text, setCookies } = await requestToken(email);
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success"}');
    expect(setCookies).toEqual([]);
    expect(await userCount()).toBe(users + 1);

    const row = (await userRowByEmail(server, email))!;
    expect(row.user_id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    expect(row.username).toBe(`user_${row.user_id.slice(0, 8)}`);
    expect(row.token).toMatch(/^[0-9a-f]{64}$/);
    expect(isAbout(row.expiry!, HOUR)).toBe(true);
    expect(row).toMatchObject({
      password: null,
      password_version: 1,
      email,
      is_deleted: false,
      imap_uid_validity: null,
      auth_sub: null,
    });
    expect(Math.abs(row.updated.getTime() - Date.now())).toBeLessThan(15_000);

    const [mail, ...others] = mailsTo(email);
    expect(others).toEqual([]);
    expect(mail).toMatchObject({
      method: "POST",
      path: "/v3/mail.test/messages",
      authorization: `Basic ${Buffer.from(`api:${MAILGUN_KEY}`).toString("base64")}`,
      fieldOrder: ["from", "to", "subject", "html", "text"],
      fields: {
        from: ["Mail <no-reply@mail.test>"],
        to: [email],
        subject: ["Please set your password for Mail"],
        html: [authenticationHtml(`https://app.mail.test/set-info/${email}?t=${row.token}`)],
        text: [AUTHENTICATION_TEXT],
      },
      attachments: [],
    });
  });

  test("leaves no copy of the mail in admin's mailbox", async () => {
    await requestToken("nocopy@elsewhere.example");
    expect(mailsTo("nocopy@elsewhere.example")).toHaveLength(1);
    expect(await server.db.query("SELECT 1 FROM mails")).toEqual([]);
  });

  test("puts the address into the link as it was given", async () => {
    const email = "First.Last+tag%x_y-z@Sub-Domain.Elsewhere.Example";
    expect((await requestToken(email)).body).toEqual(SUCCESS);
    const row = (await userRowByEmail(server, email))!;
    const mail = mailsTo(email).at(-1)!;
    expect(mail.fields.to).toEqual([email]);
    expect(mail.fields.html![0]).toContain(`href="https://app.mail.test/set-info/${email}?t=${row.token}"`);
  });

  test("accepts addresses under another user's subdomain and look-alike domains", async () => {
    for (const email of ["x@bob.mail.test", "x@notmail.test", "x@mail.test.example", "x@.b", "x@b."]) {
      const { body } = await requestToken(email);
      expect({ email, body }).toEqual({ email, body: SUCCESS });
      expect((await userRowByEmail(server, email))!.username).toMatch(/^user_[0-9a-f]{8}$/);
      expect(mailsTo(email)).toHaveLength(1);
    }
  });

  test("asking again for the same new address re-issues the token onto the same row", async () => {
    const email = "twice@elsewhere.example";
    expect((await requestToken(email)).body).toEqual(SUCCESS);
    const first = (await userRowByEmail(server, email))!;
    const users = await userCount();

    const second = await requestToken(email);
    expect(second.status).toBe(200);
    expect(second.text).toBe('{"status":"success"}');
    expect(second.setCookies).toEqual([]);

    const row = (await userRowByEmail(server, email))!;
    expect(await userCount()).toBe(users);
    expect(row.token).toMatch(/^[0-9a-f]{64}$/);
    expect(row.token).not.toBe(first.token);
    expect(isAbout(row.expiry!, HOUR)).toBe(true);
    expect(row).toMatchObject({
      user_id: first.user_id,
      username: first.username,
      password: null,
      password_version: 1,
      email,
    });

    // The same mail as the first, with the new token: the placeholder is
    // nobody's chosen name, so the link leaves it out and the person picks one.
    const [, again, ...others] = mailsTo(email);
    expect(others).toEqual([]);
    expect(again!.fields.html).toEqual([authenticationHtml(`https://app.mail.test/set-info/${email}?t=${row.token}`)]);
  });

  test("answers 500 and keeps the placeholder row when Mailgun refuses the mail; asking again mails a new token", async () => {
    const email = "bounced@elsewhere.example";
    server.mailgun.replyOnce(() => Response.json({ message: "Forbidden" }, { status: 401 }));
    const { status, body } = await requestToken(email);
    expect(status).toBe(500);
    expect(body).toEqual(INTERNAL_ERROR);
    expect(mailsTo(email)).toHaveLength(1);
    const row = (await userRowByEmail(server, email))!;
    expect(row.token).toMatch(/^[0-9a-f]{64}$/);
    expect(row.password).toBeNull();

    // The address is not stranded: the next request reuses the row.
    expect((await requestToken(email)).body).toEqual(SUCCESS);
    const retried = (await userRowByEmail(server, email))!;
    expect(retried.user_id).toBe(row.user_id);
    expect(retried.token).not.toBe(row.token);
    expect(mailsTo(email)).toHaveLength(2);
    expect(mailsTo(email)[1]!.fields.html![0]).toContain(`?t=${retried.token}"`);
  });
});

describe("POST /api/users/token — the link's host", () => {
  // NOTE: the link is built from APP_HOSTNAME alone, with no fallback to the
  // mail domain, so without it the mail carries "https://undefined/…".
  test("is the text 'undefined' when APP_HOSTNAME is unset", async () => {
    const bare = await startServer({ production: true, env: { APP_HOSTNAME: undefined } });
    try {
      const email = "nohost@elsewhere.example";
      expect((await bare.client().post("/api/users/token", { email })).body).toEqual(SUCCESS);
      const row = (await userRowByEmail(bare, email))!;
      expect(bare.mailgun.requests).toHaveLength(1);
      expect(bare.mailgun.requests[0]!.fields.html).toEqual([
        authenticationHtml(`https://undefined/set-info/${email}?t=${row.token}`),
      ]);
    } finally {
      await bare.stop();
    }
  }, 90_000);
});

describe("POST /api/users/set-info — what it refuses", () => {
  const valid = { email: "someone@elsewhere.example", username: "someone", password: "a-password", token: "t" };
  const setInfo = (body: unknown) => server.client().post("/api/users/set-info", body);

  test("a body that is an array", async () => {
    for (const body of [[], [valid]]) {
      const result = await setInfo(body);
      expect(result.status).toBe(200);
      expect(result.body).toEqual(INVALID_BODY);
    }
  });

  test.each([...NON_STRINGS, ["an empty string", ""], ["missing", undefined]])("an email that is %s", async (_l, email) => {
    const { status, body } = await setInfo({ ...valid, email });
    expect(status).toBe(200);
    expect(body).toEqual({ status: "failed", message: "email is required and must be a string." });
  });

  test.each([...NON_STRINGS, ["an empty string", ""], ["missing", undefined]])("a username that is %s", async (_l, username) => {
    const { status, body } = await setInfo({ ...valid, username });
    expect(status).toBe(200);
    expect(body).toEqual({ status: "failed", message: "username is required and must be a string." });
  });

  test.each([...NON_STRINGS, ["an empty string", ""], ["missing", undefined]])("a password that is %s", async (_l, password) => {
    const { status, body } = await setInfo({ ...valid, password });
    expect(status).toBe(200);
    expect(body).toEqual({ status: "failed", message: "password is required and must be a string." });
  });

  test.each(NON_STRINGS)("a token that is %s", async (_label, token) => {
    const { status, body } = await setInfo({ ...valid, token });
    expect(status).toBe(200);
    expect(body).toEqual({ status: "failed", message: "token must be a string." });
  });

  test("the fields are checked in order: email, username, password, token", async () => {
    expect((await setInfo({})).body.message).toBe("email is required and must be a string.");
    expect((await setInfo({ email: "a@b.co" })).body.message).toBe("username is required and must be a string.");
    expect((await setInfo({ email: "a@b.co", username: "a" })).body.message).toBe(
      "password is required and must be a string."
    );
    expect((await setInfo({ email: "a@b.co", username: "a", password: "p", token: 1 })).body.message).toBe(
      "token must be a string."
    );
    expect((await server.client().request("POST", "/api/users/set-info")).body.message).toBe(
      "email is required and must be a string."
    );
  });

  test("an address nobody asked a token for (500)", async () => {
    const { status, body, setCookies } = await setInfo({ ...valid, email: "stranger@elsewhere.example" });
    expect(status).toBe(500);
    expect(body).toEqual(INTERNAL_ERROR);
    expect(setCookies).toEqual([]);
    expect(await userRowByEmail(server, "stranger@elsewhere.example")).toBeUndefined();
  });

  test("an account whose address someone else reads, before anything is written", async () => {
    // A viewer of mail.test could have read the token mailed to admin.
    const viewer = await createUser(server, "onlooker");
    await server.db.query(
      `INSERT INTO domain_roles (domain_id, user_id, role)
       SELECT domain_id, $1, 'viewer' FROM domains WHERE name = 'mail.test'`,
      [viewer.id]
    );
    await server.db.query("UPDATE users SET token = 'seeded-token', expiry = NOW() + INTERVAL '1 hour' WHERE username = 'admin'");
    try {
      const before = (await userRow(server, "admin"))!;
      const result = await setInfo({ email: before.email, username: "admin", password: "taken-over", token: "seeded-token" });
      expect(result.status).toBe(200);
      expect(result.body).toEqual(INVALID_CREDENTIALS);
      expect(result.setCookies).toEqual([]);
      expect((await userRow(server, "admin"))!).toMatchObject({
        password: before.password,
        password_version: before.password_version,
        token: "seeded-token",
      });
      const login = await server.client().post("/api/users/login", { username: "admin", password: ADMIN_PASSWORD });
      expect(login.body.status).toBe("success");
    } finally {
      await server.db.query("DELETE FROM domain_roles WHERE user_id = $1", [viewer.id]);
      await server.db.query("UPDATE users SET token = NULL, expiry = NULL WHERE username = 'admin'");
    }
  });
});

describe("POST /api/users/set-info — resetting a password", () => {
  let sequence = 0;
  /** A registered user with a reset token on the row. */
  const userWithToken = async (expiry = "NOW() + INTERVAL '1 hour'") => {
    const user = await createUser(server, `reset${++sequence}`);
    const token = `reset-token-${sequence}`;
    await server.db.query(`UPDATE users SET token = $2, expiry = ${expiry} WHERE user_id = $1`, [user.id, token]);
    return { user, token };
  };

  test("sets the password, moves the credential generation on and signs the caller in", async () => {
    const { user, token } = await userWithToken();
    const before = (await userRow(server, user.username))!;
    const other = await createUser(server, "bystander");

    // Two sessions of the user, one of somebody else.
    const phone = server.client();
    const laptop = server.client();
    const bystander = server.client();
    await phone.login(user.username, user.password);
    await laptop.login(user.username, user.password);
    await bystander.login(other.username, other.password);
    const oldSessions = [sessionIdOf(phone), sessionIdOf(laptop)];

    const client = server.client();
    const result = await client.post("/api/users/set-info", {
      email: user.email,
      username: "a-name-that-is-ignored",
      password: "the-new-password",
      token,
    });
    expect(result.status).toBe(200);
    expect(result.text).toBe(
      `{"status":"success","body":{"id":"${user.id}","email":"${user.email}","username":"${user.username}","passwordVersion":2}}`
    );

    const row = (await userRow(server, user.username))!;
    expect(row.password).toMatch(/^\$2b\$10\$[./A-Za-z0-9]{53}$/);
    expect(row.password).not.toBe(before.password);
    expect(row).toMatchObject({
      user_id: user.id,
      username: user.username,
      email: user.email,
      password_version: 2,
      token: null,
      expiry: null,
    });
    expect(row.updated.getTime()).toBeGreaterThan(before.updated.getTime());

    // Every session the old password issued is gone; other users keep theirs.
    for (const id of oldSessions) expect(await sessionRow(server, id)).toBeUndefined();
    expect((await phone.get("/api/users/login")).body.body.user).toBeUndefined();
    expect((await bystander.get("/api/users/login")).body.body.user.username).toBe("bystander");

    // The caller holds a new session on the new generation.
    const cookie = onlySetCookie(result);
    expectSessionCookie(cookie, { secure: true });
    const session = (await sessionRow(server, sessionIdOfValue(cookie.value)))!;
    expect(session).toMatchObject({
      session_user_id: user.id,
      session_username: user.username,
      session_email: user.email,
      session_password_version: 2,
      session_is_read_only: null,
    });
    expect((await client.get("/api/users/login")).body.body.user).toEqual({
      id: user.id,
      email: user.email,
      username: user.username,
      passwordVersion: 2,
    });

    // The new password signs in, the old one does not.
    const withNew = await server.client().post("/api/users/login", {
      username: user.username,
      password: "the-new-password",
    });
    expect(withNew.body.body).toEqual({ id: user.id, email: user.email, username: user.username, passwordVersion: 2 });
    const withOld = await server.client().post("/api/users/login", { username: user.username, password: user.password });
    expect(withOld.body).toEqual(INVALID_CREDENTIALS);
  });

  test("a token works once", async () => {
    const { user, token } = await userWithToken();
    const body = { email: user.email, username: user.username, password: "first-new-password", token };
    expect((await server.client().post("/api/users/set-info", body)).body.status).toBe("success");
    const again = await server.client().post("/api/users/set-info", { ...body, password: "second-new-password" });
    expect(again.status).toBe(500);
    expect(again.body).toEqual(INTERNAL_ERROR);
    expect((await userRow(server, user.username))!.password_version).toBe(2);
    const login = await server.client().post("/api/users/login", { username: user.username, password: "first-new-password" });
    expect(login.body.status).toBe("success");
  });

  test("a row that predates credential generations moves from 1 to 2", async () => {
    const { user, token } = await userWithToken();
    await server.db.query("UPDATE users SET password_version = NULL WHERE user_id = $1", [user.id]);
    const result = await server.client().post("/api/users/set-info", {
      email: user.email,
      username: user.username,
      password: "new-password",
      token,
    });
    expect(result.body.body.passwordVersion).toBe(2);
    expect((await userRow(server, user.username))!.password_version).toBe(2);
  });

  test("only the first 72 bytes of the new password are kept", async () => {
    const { user, token } = await userWithToken();
    const password = `${"n".repeat(72)}and-a-tail`;
    const result = await server.client().post("/api/users/set-info", {
      email: user.email,
      username: user.username,
      password,
      token,
    });
    expect(result.body.status).toBe("success");
    const cut = await server.client().post("/api/users/login", { username: user.username, password: "n".repeat(72) });
    expect(cut.body.status).toBe("success");
    const short = await server.client().post("/api/users/login", { username: user.username, password: "n".repeat(71) });
    expect(short.body).toEqual(INVALID_CREDENTIALS);
  });

  test("a unicode password round-trips", async () => {
    const { user, token } = await userWithToken();
    const password = "nöuveau mot de passe 密码 🔐";
    const result = await server.client().post("/api/users/set-info", {
      email: user.email,
      username: user.username,
      password,
      token,
    });
    expect(result.body.status).toBe("success");
    const login = await server.client().post("/api/users/login", { username: user.username, password });
    expect(login.body.status).toBe("success");
  });

  test("a caller who was signed in as someone else loses that session", async () => {
    const { user, token } = await userWithToken();
    const other = await createUser(server, "visitor");
    const client = server.client();
    await client.login(other.username, other.password);
    const previous = sessionIdOf(client);
    const result = await client.post("/api/users/set-info", {
      email: user.email,
      username: user.username,
      password: "new-password",
      token,
    });
    expect(result.body.status).toBe("success");
    expect(sessionIdOf(client)).not.toBe(previous);
    expect(await sessionRow(server, previous)).toBeUndefined();
    expect((await client.get("/api/users/login")).body.body.user.username).toBe(user.username);
  });

  test("a wrong, empty or missing token is a 500 and changes nothing", async () => {
    const { user, token } = await userWithToken();
    const before = (await userRow(server, user.username))!;
    const session = server.client();
    await session.login(user.username, user.password);

    for (const attempt of [`${token}x`, token.toUpperCase(), "", undefined]) {
      const result = await server.client().post("/api/users/set-info", {
        email: user.email,
        username: user.username,
        password: "new-password",
        token: attempt,
      });
      expect({ attempt, status: result.status, body: result.body }).toEqual({
        attempt,
        status: 500,
        body: INTERNAL_ERROR,
      });
      expect(result.setCookies).toEqual([]);
    }
    const after = (await userRow(server, user.username))!;
    expect(after).toMatchObject({ password: before.password, password_version: 1, token });
    expect(after.expiry).toEqual(before.expiry);
    expect((await session.get("/api/users/login")).body.body.user.username).toBe(user.username);
  });

  test("a registered user with no token pending cannot be reset, even with an empty token", async () => {
    const user = await createUser(server, "notoken");
    for (const token of [undefined, "", "guess"]) {
      const result = await server.client().post("/api/users/set-info", {
        email: user.email,
        username: user.username,
        password: "new-password",
        token,
      });
      expect(result.status).toBe(500);
      expect(result.body).toEqual(INTERNAL_ERROR);
    }
    await server.db.query("UPDATE users SET token = '' WHERE user_id = $1", [user.id]);
    const emptyStored = await server.client().post("/api/users/set-info", {
      email: user.email,
      username: user.username,
      password: "new-password",
      token: "",
    });
    expect(emptyStored.status).toBe(500);
    const login = await server.client().post("/api/users/login", { username: user.username, password: user.password });
    expect(login.body.status).toBe("success");
  });

  test("an expired token is a 500, and is cleared off the account", async () => {
    const { user, token } = await userWithToken("NOW() - INTERVAL '1 second'");
    const before = (await userRow(server, user.username))!;
    const result = await server.client().post("/api/users/set-info", {
      email: user.email,
      username: user.username,
      password: "new-password",
      token,
    });
    expect(result.status).toBe(500);
    expect(result.body).toEqual(INTERNAL_ERROR);
    expect(result.setCookies).toEqual([]);
    const after = (await userRow(server, user.username))!;
    expect(after).toMatchObject({ password: before.password, password_version: 1, token: null, expiry: null });
    const login = await server.client().post("/api/users/login", { username: user.username, password: user.password });
    expect(login.body.status).toBe("success");
  });

  test("a token without an expiry does not expire", async () => {
    const { user, token } = await userWithToken("NULL");
    const result = await server.client().post("/api/users/set-info", {
      email: user.email,
      username: user.username,
      password: "new-password",
      token,
    });
    expect(result.body.status).toBe("success");
  });

  test("the token of a /token request works end to end", async () => {
    const user = await createUser(server, "fullflow");
    expect((await requestToken(user.email)).body).toEqual(SUCCESS);
    const token = (await userRow(server, user.username))!.token!;
    const mail = mailsTo(user.email).at(-1)!;
    expect(mail.fields.html![0]).toContain(`/set-info/${user.email}?t=${token}&u=fullflow`);
    const client = server.client();
    const result = await client.post("/api/users/set-info", {
      email: user.email,
      username: "another-name",
      password: "chosen-after-the-mail",
      token,
    });
    // A reset never renames the account.
    expect(result.body.body).toEqual({ id: user.id, email: user.email, username: "fullflow", passwordVersion: 2 });
    expect((await client.get("/api/users/login")).body.body.user.username).toBe("fullflow");
    expect(await userRow(server, "another-name")).toBeUndefined();
  });
});

describe("POST /api/users/set-info — the username", () => {
  test("is ignored for an account that already has one", async () => {
    const user = await createUser(server, "keepsname");
    await createUser(server, "takenname");
    await server.db.query("UPDATE users SET token = 'keep-token' WHERE user_id = $1", [user.id]);
    const result = await server.client().post("/api/users/set-info", {
      email: user.email,
      username: "takenname",
      password: "new-password",
      token: "keep-token",
    });
    expect(result.body.body.username).toBe("keepsname");
    expect((await userRow(server, "keepsname"))!.password_version).toBe(2);
    expect((await userRow(server, "takenname"))!.password_version).toBe(1);
  });

  test("is taken from the request by a row whose own username is empty", async () => {
    await createUser(server, "occupied");
    const [row] = await server.db.query<{ user_id: string }>(
      "INSERT INTO users (username, password, email, token) VALUES ('', 'x', 'nameless@elsewhere.example', 'nameless-token') RETURNING user_id"
    );
    const body = { email: "nameless@elsewhere.example", password: "new-password", token: "nameless-token" };

    const taken = await server.client().post("/api/users/set-info", { ...body, username: "occupied" });
    expect(taken.status).toBe(500);
    expect(taken.body).toEqual(INTERNAL_ERROR);
    expect((await userRowByEmail(server, "nameless@elsewhere.example"))!).toMatchObject({
      username: "",
      token: "nameless-token",
    });

    const free = await server.client().post("/api/users/set-info", { ...body, username: "Fresh Name" });
    expect(free.status).toBe(200);
    expect(free.body.body).toEqual({
      id: row!.user_id,
      email: "nameless@elsewhere.example",
      username: "Fresh Name",
      passwordVersion: 2,
    });
    expect((await userRow(server, "Fresh Name"))!.user_id).toBe(row!.user_id);
  });
});

describe("POST /api/users/set-info — completing a new sign-up", () => {
  /** A sign-up as the token route leaves it: the placeholder row and its token. */
  const signUp = async (email: string) => {
    expect((await requestToken(email)).body).toEqual(SUCCESS);
    const placeholder = (await userRowByEmail(server, email))!;
    expect(placeholder.password).toBeNull();
    return placeholder;
  };

  /** Signs in over IMAP and answers the tagged status. */
  const imapLogin = async (username: string, password: string) => {
    const imap = await ImapClient.connect(server);
    try {
      return (await imap.command(`LOGIN ${username} ${password}`)).status;
    } finally {
      imap.close();
    }
  };

  test("sets the chosen username and the first password on the placeholder row, and signs the caller in", async () => {
    const email = "hopeful@elsewhere.example";
    const placeholder = await signUp(email);
    const id = placeholder.user_id;
    const username = "hopeful";

    const client = server.client();
    const result = await client.post("/api/users/set-info", {
      email,
      username,
      password: "a-first-password",
      token: placeholder.token,
    });
    expect(result.status).toBe(200);
    expect(result.text).toBe(
      `{"status":"success","body":{"id":"${id}","email":"${email}","username":"${username}","passwordVersion":2}}`
    );

    // The same row, now named as asked and with a password: the placeholder
    // name was nobody's choice.
    const row = (await userRowByEmail(server, email))!;
    expect(row.password).toMatch(/^\$2b\$10\$[./A-Za-z0-9]{53}$/);
    expect(row).toMatchObject({ user_id: id, username, password_version: 2, token: null, expiry: null });
    expect(await userRow(server, placeholder.username)).toBeUndefined();

    const cookie = onlySetCookie(result);
    expectSessionCookie(cookie, { secure: true });
    expect((await sessionRow(server, sessionIdOfValue(cookie.value)))!).toMatchObject({
      session_user_id: id,
      session_username: username,
      session_email: email,
      session_password_version: 2,
    });
    expect((await client.get("/api/users/login")).body.body.user).toEqual({ id, email, username, passwordVersion: 2 });

    // The chosen name and the password sign in from now on, over HTTP and
    // IMAP; the placeholder name does not. The token is spent.
    const login = await server.client().post("/api/users/login", { username, password: "a-first-password" });
    expect(login.body.body).toEqual({ id, email, username, passwordVersion: 2 });
    const byPlaceholder = await server.client().post("/api/users/login", {
      username: placeholder.username,
      password: "a-first-password",
    });
    expect(byPlaceholder.body).toEqual(INVALID_CREDENTIALS);
    expect(await imapLogin(username, "a-first-password")).toBe("OK");
    const again = await server.client().post("/api/users/set-info", {
      email,
      username: "hopeful",
      password: "a-second-password",
      token: placeholder.token,
    });
    expect(again.status).toBe(500);
    expect(again.body).toEqual(INTERNAL_ERROR);
  });

  test("a wrong, empty or missing token is a 500 and leaves the sign-up waiting", async () => {
    const email = "doubtful@elsewhere.example";
    const placeholder = await signUp(email);

    for (const token of ["wrong", `${placeholder.token}x`, "", undefined]) {
      const result = await server.client().post("/api/users/set-info", {
        email,
        username: "doubtful",
        password: "a-first-password",
        token,
      });
      expect({ token, status: result.status, body: result.body }).toEqual({ token, status: 500, body: INTERNAL_ERROR });
      expect(result.setCookies).toEqual([]);
    }
    const after = (await userRowByEmail(server, email))!;
    expect(after).toMatchObject({
      username: placeholder.username,
      password: null,
      password_version: 1,
      token: placeholder.token,
    });
    expect(after.expiry).toEqual(placeholder.expiry);

    // The right token still completes it.
    const result = await server.client().post("/api/users/set-info", {
      email,
      username: "doubtful",
      password: "a-first-password",
      token: placeholder.token,
    });
    expect(result.body.status).toBe("success");
  });

  test("only the latest token completes a sign-up that asked twice", async () => {
    const email = "twice-then-set@elsewhere.example";
    const first = await signUp(email);
    expect((await requestToken(email)).body).toEqual(SUCCESS);
    const second = (await userRowByEmail(server, email))!;
    expect(second.token).not.toBe(first.token);

    // The re-issued link lets the person choose a name, as the first did.
    const link = mailsTo(email).at(-1)!.fields.html![0]!;
    expect(link).toContain(`?t=${second.token}"`);
    expect(link).not.toContain("&u=");

    const body = { email, username: "twice", password: "a-first-password" };
    expect((await server.client().post("/api/users/set-info", { ...body, token: first.token })).status).toBe(500);
    const result = await server.client().post("/api/users/set-info", { ...body, token: second.token });
    expect(result.body.body).toEqual({ id: first.user_id, email, username: "twice", passwordVersion: 2 });
    expect(await imapLogin("twice", "a-first-password")).toBe("OK");
  });

  test("a name another account has is refused, and the sign-up stays pending", async () => {
    await createUser(server, "claimed");
    const email = "second-comer@elsewhere.example";
    const placeholder = await signUp(email);
    const body = { email, password: "a-first-password", token: placeholder.token };

    // `admin` has a row on this server, so it is taken too.
    for (const username of ["claimed", "admin"]) {
      const result = await server.client().post("/api/users/set-info", { ...body, username });
      expect({ username, status: result.status, body: result.body }).toEqual({ username, status: 500, body: INTERNAL_ERROR });
      expect(result.setCookies).toEqual([]);
    }
    const after = (await userRowByEmail(server, email))!;
    expect(after).toMatchObject({
      user_id: placeholder.user_id,
      username: placeholder.username,
      password: null,
      token: placeholder.token,
    });

    // The same token then completes it under a free name.
    const result = await server.client().post("/api/users/set-info", { ...body, username: "second-comer" });
    expect(result.body.body.username).toBe("second-comer");
  });

  test("admin-ro is an ordinary name: a sign-up takes it, and it reads only its own mail", async () => {
    const bare = await startServer({ production: true });
    try {
      const email = "grabber@elsewhere.example";
      expect((await bare.client().post("/api/users/token", { email })).body).toEqual(SUCCESS);
      const placeholder = (await userRowByEmail(bare, email))!;
      const result = await bare.client().post("/api/users/set-info", {
        email,
        username: "admin-ro",
        password: "a-first-password",
        token: placeholder.token,
      });
      expect(result.body.body).toMatchObject({ id: placeholder.user_id, username: "admin-ro" });
      const admin = await adminUser(bare);
      const login = await bare.client().post("/api/users/login", { username: "admin-ro", password: "a-first-password" });
      expect(login.body.body.id).toBe(placeholder.user_id);
      expect(login.body.body.id).not.toBe(admin.id);
      expect(Object.keys(login.body.body)).not.toContain("isReadOnly");
    } finally {
      await bare.stop();
    }
  }, 90_000);

  test("an expired sign-up is a 500, and its row is removed so the address can start again", async () => {
    const email = "late@elsewhere.example";
    const placeholder = await signUp(email);
    await server.db.query("UPDATE users SET expiry = NOW() - INTERVAL '1 second' WHERE user_id = $1", [placeholder.user_id]);

    const result = await server.client().post("/api/users/set-info", {
      email,
      username: "late",
      password: "a-first-password",
      token: placeholder.token,
    });
    expect(result.status).toBe(500);
    expect(result.body).toEqual(INTERNAL_ERROR);
    expect(result.setCookies).toEqual([]);
    expect(await userRowByEmail(server, email)).toBeUndefined();

    const fresh = await signUp(email);
    expect(fresh.user_id).not.toBe(placeholder.user_id);
  });
});

describe("the sign-up routes — methods and paths", () => {
  test("answer POST only", async () => {
    for (const path of ["/api/users/token", "/api/users/set-info"]) {
      for (const method of ["GET", "HEAD", "DELETE", "PUT", "PATCH"]) {
        const { status } = await server.client().request(method, path);
        expect({ method, path, status }).toEqual({ method, path, status: 404 });
      }
    }
  });

  test("answer with a trailing slash and in any letter case", async () => {
    for (const path of ["/api/users/token/", "/API/USERS/TOKEN"]) {
      expect((await server.client().post(path, { email: 1 })).body).toEqual(INVALID_EMAIL);
    }
    for (const path of ["/api/users/set-info/", "/API/Users/Set-Info"]) {
      expect((await server.client().post(path, {})).body.message).toBe("email is required and must be a string.");
    }
  });
});
