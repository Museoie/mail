/**
 * Preloaded into the Bun server only on a machine without IPv6.
 *
 * The SMTP and IMAP listeners call `server.listen(port, callback)` with no
 * host, which Bun binds to `::`. Where the kernel has no IPv6 that bind fails
 * and the mail ports never open. This gives a host-less `listen` the IPv4
 * wildcard instead, so the same server code can be driven in such a sandbox.
 * It changes nothing about how the server behaves once a port is open.
 */
import net from "net";

type Listen = (this: net.Server, ...args: unknown[]) => net.Server;

const original = net.Server.prototype.listen as unknown as Listen;

const patchedListen: Listen = function (...args) {
  const [port, second] = args;
  if (typeof port === "number" && (second === undefined || typeof second === "function")) {
    return original.call(this, port, "0.0.0.0", ...args.slice(1));
  }
  return original.apply(this, args);
};

net.Server.prototype.listen = patchedListen as unknown as typeof net.Server.prototype.listen;
