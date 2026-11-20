If these instructions are outdated, please raise an issue or send us a Pull Request.

# How to Setup

You need these to use Mail:

- A domain name
- A server to run Mail

If you want to send emails using Mail. You need a 3rd party email sending service.(Currently we're using [Mailgun](https://mailgun.com))

For detailed instruction, please keep reading this document.

## 1. Install Mail

1. Clone this git repository
   ```
   git clone https://github.com/hoiekim/mail.git
   ```
2. Copy `.env.example` file and name it `.env.local` then determine environment variables in `.env.local` file as following.

   ```
   EMAIL_DOMAIN             // Domain name to use when sending & receiving mails.
   APP_HOSTNAME             // Domain name that hosts the web app.

   SECRET                   // Private value that signs session cookies. Required when
                            // NODE_ENV=production: the server refuses to boot when it is
                            // unset or set to a value this repository publishes, because
                            // either one lets anyone forge a signed cookie. Generate one
                            // with `openssl rand -base64 32`.
   ADMIN_PASSWORD           // Password the admin user is created with, on first run.

   ADMIN_PASSWORD_RESET     // (optional) Set to 1 for one boot to reset admin's password
                            // to ADMIN_PASSWORD. Remove it afterwards.

   MAILGUN_KEY              // (optional) API key issued by Mailgun. Used to send emails.

   OPENAI_KEY               // (optional) API key issued by OpenAI. Used to get insight of emails.

   PUSH_VAPID_PUBLIC_KEY    // (optional) API key issued by Push. Used to send push notifications.
   PUSH_VAPID_PRIVATE_KEY   // (optional) API key issued by Push. Used to send push notifications.

   SSL_CERTIFICATE=         // (optional) SSL certificate file path. Used for TLS in IMAP server.
   SSL_CERTIFICATE_KEY=     // (optional) SSL certificate key file path. Used for TLS in IMAP server.

   AUTH_ISSUER              // (optional) Turns on single sign-on with the AUTH_* settings
                            // in .env.example. See "Single sign-on (optional)" below.
   ```

## 2. Setup DNS Records

Make sure your domain's MX record points to the server you're running Mail. In order to setup your MX record, check your DNS settings in your domain's provider.

- Exmaple (assuming your domain name is `domain.com` and your server ip is `0.0.0.0`):
  |Type|Name|Key|Meaning|
  |----|----|---|-------|
  |A|mail|0.0.0.0|It points request for `mail.domain.com` to `0.0.0.0`|
  |MX|@|mail.domain.com|It points emails sent to `*@domain.com` and `*@*.domain.com` to `mail.domain.com`|

In the example above, `A` record is pointing `mail.domain.com` to `0.0.0.0` and `MX` record is pointing emails to `mail.domain.com`. When some email is sent to `something@domain.com`, it will look up `domain.com`'s `MX` record and send the email data to where it points to. So it will be eventually delivered to `0.0.0.0`

## 3. Setup Mailgun

1. Go to [Mailgun](https://mailgun.com) and follow instructions to get started.
2. Get the API key and paste it in `.env.local` file.

If you want to use this app only for receiving mails, skip this step.

## 4. Run app

1. Production mode

   Make sure you have docker and docker-compose installed in your machine.

   ```
   docker-compose up
   ```

2. Development mode

   Set following values in `.env.local` file to configure PostgreSQL for development. You can install and run PostgreSQL locally or use the docker-compose setup.

   ```
   POSTGRES_HOST=localhost  // PostgreSQL host address
   POSTGRES_PORT=5432       // PostgreSQL port
   POSTGRES_USER=postgres   // PostgreSQL username
   POSTGRES_PASSWORD=inbox  // PostgreSQL password
   POSTGRES_DATABASE=mail   // PostgreSQL database name
   ```

   Then run app using this command

   ```
   bun install
   bun run dev
   ```

### Running behind a reverse proxy

In production the app resolves each request's client address through Express's
`trust proxy` setting, which is fixed to the loopback and private ranges. A
forwarding header is honoured only when the connection itself arrives from one
of those addresses — in other words, from the proxy. A request that reaches the
app port from anywhere else is attributed to its own socket address and its
forwarding headers are ignored.

Rate limits are keyed on that address, so the proxy has to pass the real client
address along or every client ends up sharing one bucket. With nginx:

```
proxy_set_header X-Forwarded-For $remote_addr;
proxy_set_header X-Forwarded-Proto $scheme;
```

`X-Forwarded-Proto` is what lets the session cookie's `Secure` attribute apply;
without it Express sees a plaintext request and declines to set the cookie.
`X-Real-IP` is not read — set `X-Forwarded-For` even if you already set it.

Nothing other than the proxy needs to reach the app's HTTP port, so there is no
reason to publish it on a public interface.

### Mail ports behind a TCP proxy

The SMTP and IMAP ports can sit behind a TCP proxy too, such as nginx's
`stream` module, so that two copies of the server can run side by side while
one replaces the other. Set `PROXY_PROTOCOL=true`, and have the proxy open
every connection with a PROXY protocol v1 header:

```
stream {
  server {
    listen 25;
    proxy_pass mail:25;
    proxy_protocol on;
  }
  # ...one server per mail port. Set proxy_timeout above 35 minutes, or the
  # proxy cuts IMAP IDLE connections (nginx's default is 10 minutes).
}
```

The header is accepted only from the same loopback and private ranges as the
HTTP forwarding headers. That doesn't stop anything else on those ranges from
sending one: another container on the network, or a client reaching a port
that is still published through Docker's port forwarding, can then name any
client address. With the setting on, only the proxy should reach the mail
ports. TLS is unaffected: the proxy passes it through, and the server
terminates it as before.

### Environment Variables

This app in default uses `.env` and `.env.local` to load environment variables. `.env` is included in the repository, intending to determine consistent variables that are related to React's build process, etc. `.env.local` is not included in the repository, intending to determine variables that differ by the app's host environment, external API credentials, etc. Additionally, we have an option to add another one as `.env.<NODE_ENV>` where you can set `NODE_ENV` in your terminal for example in Mac/Linux, `NODE_ENV=development` or in Windows cmd, `set NODE_ENV=development`.

## 5. Enjoy!

Default port number is 3004. So you can connect to Mail at http://(your-server-ip):3004

For development mode, use port number 3000 instead.

Admin username is `admin`. On the first run its password is the value of the environment variable called `ADMIN_PASSWORD`. After that, the password stored in the database wins — changing `ADMIN_PASSWORD` and restarting does not overwrite a password you changed in the app. To reset a password you have lost, boot once with `ADMIN_PASSWORD_RESET=1` and then remove that variable.

## Single sign-on (optional)

Mail can sign people in through an OpenID Connect identity provider instead of
its own passwords: the web app sends the browser to the provider, and mail apps
(IMAP and SMTP clients, which can only carry a username and a password) use
app passwords that people create at the provider. It talks to the provider only
through its documented interfaces: the OpenID Connect endpoints, the signed
events it posts, and app-password introspection, as the auth service offers
them.

It is off unless `AUTH_ISSUER` is set. Off, Mail behaves exactly as described
above and never contacts a provider. On, the server checks the settings at
start and refuses to start, listing every problem, when one is missing or
malformed. It still never contacts the provider at start: the first sign-in
does, so a provider that is down does not stop Mail from starting.

### Settings

| Variable | Required | Meaning |
|---|---|---|
| `AUTH_ISSUER` | — | Issuer URL of the provider, e.g. `https://auth.example.com`. Unset or empty turns single sign-on off. |
| `AUTH_CLIENT_ID` | with `AUTH_ISSUER` | The OAuth client id registered for Mail. |
| `AUTH_CLIENT_SECRET` | with `AUTH_ISSUER` | That client's secret. |
| `AUTH_REDIRECT_URI` | no | The callback registered for the client. Default: `https://<APP_HOSTNAME>/api/auth/callback`; required when `APP_HOSTNAME` is unset. |
| `AUTH_BACKCHANNEL_URL` | no | A private-network URL of the same provider for server-to-server calls. |
| `AUTH_AUDIENCE` | no | The service name in tokens' `aud`, when it differs from the client id. |
| `AUTH_LOCAL_LOGIN` | no, default `false` | `true` keeps Mail's own passwords working alongside single sign-on: password sign-in, sign-up and reset in the web app, and local passwords on IMAP and SMTP. Meant for a migration period. |
| `AUTH_LINK_USERS` | no | Comma-separated `<mail username>=<provider username>` pairs that link existing accounts on their first single sign-on, e.g. `admin=alice,bob=bob`. |

After signing out, the provider sends the browser back to the origin of the
redirect URI, `https://<APP_HOSTNAME>/`, so register that as a post-logout
redirect URI.

### Registering Mail with the provider

With the auth service, register Mail on its admin page (the **Services** tab),
or in its services file if the deployment keeps one. Once the client's secret
is made, the admin page lists the `AUTH_*` lines to paste into Mail's
environment. Mail has no roles of its own: any role on the service lets the
person use Mail, and removing it keeps them out. App passwords need the `imap`
and `smtp` scopes. The definition, as the services file takes it:

```json
{
  "name": "mail",
  "roles": ["user"],
  "mfa_roles": [],
  "signup": "invite",
  "app_password_scopes": ["imap", "smtp"],
  "clients": [{
    "id": "mail",
    "type": "confidential",
    "redirect_uris": ["https://mail.example.com/api/auth/callback"],
    "post_logout_redirect_uris": ["https://mail.example.com/"]
  }],
  "events_url": "https://mail.example.com/api/auth/events"
}
```

`events_url` can be a private-network address instead, such as
`http://mail.internal:3004/api/auth/events`: the provider posts signed events
there when someone is disabled, deleted, removed from the service or signs out,
or revokes an app password. The route takes no session, so it has to stay
reachable through your reverse proxy if the provider calls the public address.
Set `AUTH_AUDIENCE` if the service's `name` is not the client's `id`.

### Mail apps: app passwords

IMAP and SMTP clients cannot follow a browser sign-in, so with single sign-on
on they sign in with an app password: each person creates one in the
provider's account page, for the `mail` service with the `imap` and `smtp`
scopes, and enters it in their mail app as the password. The username stays
their Mail username (`bob`, not an email address). Mail checks the app password
with the provider, then that it belongs to the account linked to that person,
that the username names that account, and that its scopes cover the protocol.

- While single sign-on is on, IMAP and SMTP submission take no password before
  TLS: use port 993 or 465, or STARTTLS on 143 and 587. An app password is a
  full credential for the protocol. Without a TLS certificate configured, mail
  apps cannot sign in at all.
- When the provider cannot be reached, IMAP answers `NO [UNAVAILABLE]` and SMTP
  `454`, and the attempt is not counted against the client's failed-attempt
  limit. An app password checked in the last ten minutes keeps working through
  the outage.
- Local passwords keep working on IMAP and SMTP only while `AUTH_LOCAL_LOGIN=true`,
  so a migration can give people time to create app passwords before the
  setting is removed. Mail tries the local password first then, so a matching
  one is never sent to the provider and doesn't use up the provider's limits on
  failed checks.

### How existing accounts are linked

The first single sign-on of a person picks their Mail account in this order:

1. the account already linked to them;
2. the account `AUTH_LINK_USERS` names for their provider username, if it is
   not linked yet. This is the only way the `admin` account is linked (for
   example `AUTH_LINK_USERS=admin=alice`);
3. the one account, not linked yet, whose sign-up email is the email address
   the provider gives for them, ignoring case, whether or not the provider
   marks it verified. When two or more such accounts have that address, none
   is picked. A sign-up that was never finished, a deleted account, `admin-ro`
   (a local account only) and `admin` are never picked. Email never links
   `admin` because mail to its address, `admin@<EMAIL_DOMAIN>`, can be read by
   the read-only `admin-ro` login: whoever holds that login could confirm that
   address at the provider and take `admin` over;
4. otherwise a new account, named after their provider username. It is refused
   (`An account with your username already exists here`) when an account by
   that name exists but is not linked to them: link it with `AUTH_LINK_USERS`
   instead. A provider username that cannot be a mailbox name (one lower-case
   DNS label) is refused too.

The email match in step 3 is as trustworthy as `AUTH_LINK_USERS`: Mail's
sign-up proved the account's address, and the auth service sets an address a
person chose only after they confirm it (an address it has not verified was
entered by its administrator, when they created or imported the person). Don't
use Mail's single sign-on with a provider that lets people claim addresses
they haven't confirmed: anyone could then link themselves to the account that
signed up with an address they claimed.

A new account's address is the email address the provider gives for them,
verified or not, or, without one, its own mailbox address. Each sign-in copies
a changed address onto the account only when the provider marks it verified,
and never onto `admin` or `admin-ro`, whose address the boot sets.

### What changes while it is on

- The sign-in page shows **Sign in with SSO**. Unless `AUTH_LOCAL_LOGIN=true`,
  the password form, sign-up and password reset are hidden, the server refuses
  them with `403`, and sessions that were signed in with a password before
  single sign-on was turned on end at their next request.
- Every web session re-checks access with the provider every ten minutes. A
  session the provider no longer accepts ends; while the provider cannot be
  reached the session keeps working for up to an hour past its re-check.
- Signing out also ends the session at the provider.
- When the provider reports that someone was disabled, deleted or removed from
  the service, or revoked an app password, Mail signs that account out of the
  web and closes its IMAP and SMTP connections — the ones on other copies of
  the server at their next command. Revoking one app password closes the
  account's other mail connections too; mail apps reconnect on their own.
  Mail never deletes an account's mail: after `user.deleted`, remove the data
  yourself if you want it gone.
- While `AUTH_LOCAL_LOGIN=true`, an account's own password keeps signing it in
  even after the provider has removed the person; remove the setting once the
  migration is done to make the provider the only way in.
- `admin-ro` is a local account only and signs in only while
  `AUTH_LOCAL_LOGIN=true`. `ADMIN_PASSWORD` still seeds `admin`'s password.

### Turning it off

Unset `AUTH_ISSUER` and restart. Password sign-in works again for every account
that had a password, and sessions that came from single sign-on end. Accounts
that single sign-on created have no password anyone knows: they set one with
**Forgot your password?**, which mails a link to the account's address (for an
account created without an email address from the provider, that is its own
mailbox, which its owner cannot read until they can sign in again).
