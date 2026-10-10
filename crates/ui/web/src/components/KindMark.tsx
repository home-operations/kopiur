import type { ObjectKind } from "../api/types";
import { KIND_META } from "./kind";

/**
 * The kind's tinted icon chip. Square, where a status pill is round, so the
 * two never read as each other. Decorative: the kind is said by `KindName` or
 * by a table's column header.
 */
export function KindChip({ kind, size = "md" }: { kind: ObjectKind; size?: "sm" | "md" | "lg" }) {
  const meta = KIND_META[kind];
  const Icon = meta.icon;
  return (
    <span className={`kind-chip kind-chip--${size}`} data-kind={meta.slug} aria-hidden="true">
      <Icon strokeWidth={2} />
    </span>
  );
}

/**
 * The CRD kind in small caps, in the kind's colour. Carries `label-strip__kind`
 * too, the hook the work ledger, gate list and overview tests read.
 */
export function KindName({ kind }: { kind: ObjectKind }) {
  const meta = KIND_META[kind];
  return (
    <span className="kind-name label-strip__kind" data-kind={meta.slug}>
      {meta.label}
    </span>
  );
}
