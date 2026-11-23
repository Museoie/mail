import { useContext, useState, KeyboardEventHandler } from "react";
import { useMutation, useQuery } from "react-query";

import { DomainEntry, DomainClaimEntry, DomainsGetResponse } from "server";
import { Context, call, queryClient, useIsOnline } from "client";

import "../Allowlist/index.scss";
import "./index.scss";

const queryUrl = "/api/domains";

type Result = { status: string; message?: string };

/**
 * The user's domains: the ones they own (and who views each), the ones they
 * view, and the ones they added and still have to prove with a DNS record.
 */
const Domains = () => {
  const { refreshDomains } = useContext(Context);
  const [input, setInput] = useState("");
  const [viewerInputs, setViewerInputs] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const { isOnline } = useIsOnline();
  const offlineTitle = isOnline ? undefined : "You're offline — reconnect to change your domains";

  const query = useQuery<DomainsGetResponse>(
    queryUrl,
    async () => {
      const { status, body, message } = await call.get<DomainsGetResponse>(queryUrl);
      if (status === "success") return body as DomainsGetResponse;
      throw new Error(message);
    },
    { retry: false, refetchOnWindowFocus: false, refetchInterval: false }
  );

  // Every change re-reads the list (it is small) and the domains the rest of
  // the app sends from.
  const after = (res: Result, success?: string) => {
    if (res.status !== "success") {
      setError(res.message || "Something went wrong. Try again.");
      setNotice(null);
      return false;
    }
    setError(null);
    setNotice(success ?? null);
    queryClient.invalidateQueries(queryUrl);
    refreshDomains?.();
    return true;
  };

  const add = useMutation(
    (name: string) => call.post<DomainClaimEntry, { name: string }>(queryUrl, { name }),
    {
      onSuccess: (res) => {
        if (after(res, "Publish the DNS record below, then verify.")) setInput("");
      }
    }
  );

  const verify = useMutation(
    (name: string) => call.post<DomainEntry, { name: string }>(`${queryUrl}/verify`, { name }),
    {
      onSuccess: (res) => {
        after(res, `${res.body?.name ?? "The domain"} is verified.`);
      }
    }
  );

  const remove = useMutation(
    (name: string) => call.delete<undefined>(`${queryUrl}/${encodeURIComponent(name)}`),
    {
      onSuccess: (res) => {
        after(res);
      }
    }
  );

  const addViewer = useMutation(
    ({ name, username }: { name: string; username: string }) =>
      call.post<DomainEntry, { name: string; username: string }>(`${queryUrl}/viewers`, {
        name,
        username
      }),
    {
      onSuccess: (res, { name }) => {
        if (after(res)) setViewerInputs((inputs) => ({ ...inputs, [name]: "" }));
      }
    }
  );

  const removeViewer = useMutation(
    ({ name, username }: { name: string; username: string }) =>
      call.delete<DomainEntry>(
        `${queryUrl}/viewers/${encodeURIComponent(name)}/${encodeURIComponent(username)}`
      ),
    {
      onSuccess: (res) => {
        after(res);
      }
    }
  );

  const handleAdd = () => {
    if (!isOnline) return;
    const name = input.trim();
    if (name) add.mutate(name);
  };

  const onInputKeyDown: KeyboardEventHandler<HTMLInputElement> = (e) => {
    if (e.key === "Enter") handleAdd();
  };

  const busy =
    add.isLoading ||
    verify.isLoading ||
    remove.isLoading ||
    addViewer.isLoading ||
    removeViewer.isLoading;

  const renderOwned = (domain: DomainEntry) => {
    const viewerInput = viewerInputs[domain.name] ?? "";
    return (
      <li key={domain.name} className="domain-entry">
        <div className="domain-row">
          <span className="pattern">{domain.name}</span>
          <span className="domain-role">Owner</span>
          {domain.managedByServer ? null : (
            <button
              className="text-button danger"
              disabled={busy || !isOnline}
              title={offlineTitle}
              onClick={() => remove.mutate(domain.name)}
            >
              Remove
            </button>
          )}
        </div>
        <div className="domain-viewers">
          {(domain.viewers ?? []).map((username) => (
            <span key={username} className="domain-viewer">
              {username}
              <button
                className="icon-button"
                aria-label={`Stop sharing ${domain.name} with ${username}`}
                title={offlineTitle || "Stop sharing"}
                disabled={busy || !isOnline}
                onClick={() => removeViewer.mutate({ name: domain.name, username })}
              >
                ×
              </button>
            </span>
          ))}
          <span className="allowlist-add domain-viewer-add">
            <input
              type="text"
              value={viewerInput}
              placeholder="Username"
              aria-label={`Share ${domain.name} read-only with`}
              onChange={(e) =>
                setViewerInputs((inputs) => ({ ...inputs, [domain.name]: e.target.value }))
              }
            />
            <button
              className="text-button"
              disabled={busy || !viewerInput.trim() || !isOnline}
              title={offlineTitle}
              onClick={() =>
                addViewer.mutate({ name: domain.name, username: viewerInput.trim() })
              }
            >
              Share
            </button>
          </span>
        </div>
      </li>
    );
  };

  const renderViewed = (domain: DomainEntry) => (
    <li key={domain.name} className="domain-entry">
      <div className="domain-row">
        <span className="pattern">{domain.name}</span>
        <span className="domain-role">Read-only, from {domain.owner}</span>
      </div>
    </li>
  );

  const renderClaim = (claim: DomainClaimEntry) => (
    <li key={claim.name} className="domain-entry">
      <div className="domain-row">
        <span className="pattern">{claim.name}</span>
        <span className="domain-role">Not verified</span>
        <span className="confirm">
          <button
            className="text-button"
            disabled={busy || !isOnline}
            title={offlineTitle}
            onClick={() => verify.mutate(claim.name)}
          >
            Verify
          </button>
          <button
            className="text-button danger"
            disabled={busy || !isOnline}
            title={offlineTitle}
            onClick={() => remove.mutate(claim.name)}
          >
            Remove
          </button>
        </span>
      </div>
      <div className="domain-record">
        Add a {claim.record.type} record named <code>{claim.record.host}</code> with the
        value <code>{claim.record.value}</code>.
      </div>
    </li>
  );

  const renderBody = () => {
    if (query.isLoading) return <div className="allowlist-status">Loading…</div>;
    if (query.error || !query.data) {
      return <div className="allowlist-status">Failed to load your domains.</div>;
    }
    // Tolerate a partial payload (an older server) rather than failing the page.
    const domains = query.data.domains ?? [];
    const claims = query.data.claims ?? [];
    const owned = domains.filter((domain) => domain.role === "owner");
    const viewed = domains.filter((domain) => domain.role === "viewer");
    return (
      <ul className="allowlist-entries">
        {owned.map(renderOwned)}
        {claims.map(renderClaim)}
        {viewed.map(renderViewed)}
      </ul>
    );
  };

  return (
    <div className="allowlist domains">
      <p className="allowlist-hint">
        You receive and send mail for the domains you own. To add one, add it
        here, publish the TXT record it shows, and point the domain&apos;s MX
        record at this server. Sharing a domain lets someone read its mail,
        including password-reset mail sent to it, but not send from it.
      </p>

      <div className="allowlist-add">
        <input
          type="text"
          value={input}
          placeholder="example.com"
          aria-label="Domain to add"
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={onInputKeyDown}
        />
        <button
          className="text-button"
          onClick={handleAdd}
          disabled={busy || !input.trim() || !isOnline}
          title={offlineTitle}
        >
          Add
        </button>
      </div>

      {error ? (
        <div className="allowlist-error" role="alert">
          {error}
        </div>
      ) : null}
      {notice ? (
        <div className="allowlist-status" role="status">
          {notice}
        </div>
      ) : null}

      {renderBody()}
    </div>
  );
};

export default Domains;
