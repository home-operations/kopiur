import { Link } from "@tanstack/react-router";

import type { ObjectKind } from "../api/types";
import { LampBadge } from "./HealthBadge";
import { InspectLink } from "./InspectLink";
import type { Lamp } from "./health";
import { KindChip } from "./KindMark";
import { KIND_META, detailHref, parseRef } from "./kind";

export interface ObjectRefProps {
  kind: ObjectKind;
  name: string;
  namespace?: string | undefined;
  /** The namespace the reference sits in; the target's is shown only when it differs. */
  contextNamespace?: string | undefined;
  /** The target's own state. Omitted when unknown — a pill is never guessed. */
  health?: Lamp | undefined;
  /** The target's detail route. Omitted for a kind with none: then it is not a link. */
  to?: string | undefined;
  /** Open the target in the resource drawer instead of following `to`. */
  inspect?: boolean | undefined;
}

/**
 * One object naming another: a mini card with the target's kind, its name and
 * its live status. A link when the target has a detail route; plain text when
 * it does not, because a link to a route that does not exist is a lie.
 */
export function ObjectRef({
  kind,
  name,
  namespace,
  contextNamespace,
  health,
  to,
  inspect = false,
}: ObjectRefProps) {
  const meta = KIND_META[kind];
  const showNamespace =
    namespace !== undefined && namespace.length > 0 && namespace !== contextNamespace;
  const body = (
    <>
      <KindChip kind={kind} size="sm" />
      <span className="ref__id">
        <span className="kind-name">{meta.label}</span>
        <code className="ref__name">
          {showNamespace ? <span className="ref__ns">{namespace}/</span> : null}
          {name}
        </code>
      </span>
      {health !== undefined ? <LampBadge lamp={health} /> : null}
    </>
  );
  if (inspect) {
    return (
      <InspectLink className="ref" data-kind={meta.slug} target={{ kind, name, namespace }}>
        {body}
      </InspectLink>
    );
  }
  return to !== undefined ? (
    <Link className="ref" data-kind={meta.slug} to={to}>
      {body}
    </Link>
  ) : (
    <span className="ref" data-kind={meta.slug}>
      {body}
    </span>
  );
}

/**
 * A wire reference string (`Repository/media/nas`) as an {@link ObjectRef}
 * linking to its page; anything that is not a kopiur kind stays plain mono
 * text, exactly as the server sent it.
 */
export function WireRef({
  value,
  contextNamespace,
}: {
  value: string | null | undefined;
  contextNamespace?: string | undefined;
}) {
  if (value === null || value === undefined || value.length === 0) {
    return <span className="absent">—</span>;
  }
  const ref = parseRef(value);
  if (ref === null) {
    return <span className="mono">{value}</span>;
  }
  return (
    <ObjectRef
      kind={ref.kind}
      name={ref.name}
      namespace={ref.namespace}
      contextNamespace={contextNamespace}
      to={detailHref(ref.kind, ref.name, ref.namespace)}
    />
  );
}
