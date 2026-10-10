import type { ObjectKind } from "../api/types";
import { LampBadge } from "./HealthBadge";
import { InspectLink } from "./InspectLink";
import type { Lamp } from "./health";
import { KindChip } from "./KindMark";
import { KIND_META, parseRef } from "./kind";

export interface ObjectRefProps {
  kind: ObjectKind;
  name: string;
  namespace?: string | undefined;
  /** The namespace the reference sits in; the target's is shown only when it differs. */
  contextNamespace?: string | undefined;
  /** The target's own state. Omitted when unknown — a pill is never guessed. */
  health?: Lamp | undefined;
  /** Called when the reference is followed (a popover closing behind it). */
  onOpen?: (() => void) | undefined;
}

/**
 * One object naming another: a mini card with the target's kind, its name and
 * its live status. Every kind is a link: it opens the target in the resource
 * drawer over the page you are on, whether or not the kind has a page of its
 * own.
 */
export function ObjectRef({
  kind,
  name,
  namespace,
  contextNamespace,
  health,
  onOpen,
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
  return (
    <InspectLink
      className="ref"
      data-kind={meta.slug}
      target={{ kind, name, namespace }}
      onClick={onOpen}
    >
      {body}
    </InspectLink>
  );
}

/**
 * A wire reference string (`Repository/media/nas`) as an {@link ObjectRef};
 * anything that is not a kopiur kind stays plain mono
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
    />
  );
}
