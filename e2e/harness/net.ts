import net from "net";

/** A TCP port nothing is listening on right now. */
export const freePort = (): Promise<number> =>
  new Promise((resolve, reject) => {
    const server = net.createServer();
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address() as net.AddressInfo;
      server.close(() => resolve(port));
    });
  });

/** `count` distinct free ports. */
export const freePorts = async (count: number): Promise<number[]> => {
  const ports = new Set<number>();
  while (ports.size < count) ports.add(await freePort());
  return [...ports];
};

let ipv6: Promise<boolean> | undefined;

/** Whether this machine can bind an IPv6 socket at all. */
export const hasIpv6 = (): Promise<boolean> =>
  (ipv6 ??= new Promise((resolve) => {
    const server = net.createServer();
    server.on("error", () => resolve(false));
    server.listen(0, "::", () => server.close(() => resolve(true)));
  }));

export const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** Polls `check` until it returns a value, or throws once `timeoutMs` has passed. */
export const waitFor = async <T>(
  check: () => Promise<T | undefined | null | false> | T | undefined | null | false,
  { timeoutMs = 5000, intervalMs = 25, what = "condition" } = {}
): Promise<T> => {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() > deadline) throw new Error(`Timed out after ${timeoutMs}ms waiting for ${what}`);
    await sleep(intervalMs);
  }
};
