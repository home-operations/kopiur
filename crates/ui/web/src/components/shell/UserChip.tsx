import { Check, ChevronsUpDown, CircleHelp, X } from "lucide-react";
import { useEffect } from "react";

import { useMe } from "../../api/hooks";
import { problemBanner } from "../../api/problem";
import { CAPABILITY_KEYS, CAPABILITY_LABELS, identitySourceLabel } from "../capabilities";
import { ThemeSwitch } from "./ThemeSwitch";

/**
 * Who the console is acting as, compact at the foot of the sidebar: avatar,
 * user, role (how they were identified). Opening it shows the rest — groups,
 * what they may do in this scope, and the theme switch.
 *
 * A `<details>`, so the panel's text is in the document even while closed:
 * the capability list is part of the page's account of permissions, not a
 * tooltip. A failed `/me` reads "identity unavailable", never "not permitted".
 */
export function UserChip({ namespace }: { namespace: string | undefined }) {
  const me = useMe(namespace);
  const failure = me.isError ? me.error : null;
  useEffect(() => {
    if (failure !== null) {
      problemBanner.report(failure.problem, { source: "Identity (/api/v1/me)" });
    }
  }, [failure]);

  let summary;
  let details = null;
  if (me.isPending) {
    summary = (
      <summary className="identity__summary" aria-busy="true">
        <span className="skeleton user-chip__avatar" aria-hidden="true" />
        <span className="skeleton" style={{ width: "7rem" }} aria-hidden="true" />
        <span className="visually-hidden">Loading identity…</span>
      </summary>
    );
  } else if (me.isError) {
    summary = (
      <summary className="identity__summary" data-state="error">
        <span className="user-chip__avatar" data-initial="?" aria-hidden="true" />
        <span className="identity__user">identity unavailable</span>
      </summary>
    );
  } else {
    const data = me.data;
    const allowed = CAPABILITY_KEYS.filter((key) => data.can[key]).length;
    const scopeWord = namespace !== undefined ? `in ${namespace}` : "cluster-wide";
    summary = (
      <summary className="identity__summary" aria-label={`Signed in as ${data.user}`}>
        <span
          className="user-chip__avatar"
          data-initial={data.user.charAt(0).toUpperCase()}
          aria-hidden="true"
        />
        <span className="user-chip__who">
          <span className="identity__user">{data.user}</span>
          <span className="identity__source">{identitySourceLabel(data.source)}</span>
        </span>
        <ChevronsUpDown size={14} strokeWidth={2} aria-hidden="true" />
      </summary>
    );
    details = (
      <>
        <dl className="identity__grid">
          <dt>User</dt>
          <dd>{data.user}</dd>
          <dt>Groups</dt>
          <dd>{data.groups.length > 0 ? data.groups.join(", ") : "—"}</dd>
          {data.email ? (
            <>
              <dt>Email</dt>
              <dd>{data.email}</dd>
            </>
          ) : null}
          <dt>Source</dt>
          <dd>{identitySourceLabel(data.source)}</dd>
          <dt>Scope</dt>
          <dd>{namespace ?? data.namespace ?? "cluster-wide"}</dd>
        </dl>
        <p className="identity__count">
          {allowed} of {CAPABILITY_KEYS.length} actions permitted {scopeWord}. Cluster-repository
          actions are always answered cluster-wide.
        </p>
        <ul className="cap-list" aria-label="Capabilities">
          {CAPABILITY_KEYS.map((key) => {
            const ok = data.can[key];
            return (
              <li key={key} data-allowed={ok ? "true" : "false"}>
                {ok ? (
                  <Check size={14} strokeWidth={2} aria-hidden="true" />
                ) : (
                  <X size={14} strokeWidth={2} aria-hidden="true" />
                )}
                <span>
                  {CAPABILITY_LABELS[key]}
                  <span className="visually-hidden">{ok ? ": permitted" : ": not permitted"}</span>
                </span>
              </li>
            );
          })}
        </ul>
        {data.source === "anonymous" ? (
          <p className="identity__count">
            <CircleHelp size={14} strokeWidth={1.75} aria-hidden="true" /> No identity headers
            reached kopiur-ui; every request is made as the anonymous user.
          </p>
        ) : null}
      </>
    );
  }

  return (
    <details className="identity user-chip">
      {summary}
      <div className="identity__panel">
        {details}
        <div className="identity__theme">
          <span className="identity__theme-label">Theme</span>
          <ThemeSwitch />
        </div>
      </div>
    </details>
  );
}
