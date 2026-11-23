import path from "path";

export interface TestCertificate {
  certPath: string;
  keyPath: string;
}

/**
 * Writes a self-signed certificate for `localhost` / `127.0.0.1` into `dir`.
 * Generated per run with the `openssl` CLI, so no key material is ever
 * committed. Valid for two days.
 */
export const generateCertificate = async (dir: string): Promise<TestCertificate> => {
  const certPath = path.join(dir, "tls-cert.pem");
  const keyPath = path.join(dir, "tls-key.pem");
  const openssl = Bun.spawn(
    [
      "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
      "-keyout", keyPath, "-out", certPath, "-days", "2",
      "-subj", "/CN=localhost",
      "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1",
    ],
    { stdout: "pipe", stderr: "pipe" }
  );
  const exitCode = await openssl.exited;
  if (exitCode !== 0) {
    throw new Error(`openssl could not create a test certificate: ${await new Response(openssl.stderr).text()}`);
  }
  return { certPath, keyPath };
};
