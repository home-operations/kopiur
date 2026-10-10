import { Link } from "@tanstack/react-router";
import { Stethoscope } from "lucide-react";

import { relativeTime } from "../util/format";
import type { Attention, AttentionItem, AttentionProblem } from "./attention";
import { HealthBadge, LampBadge } from "./HealthBadge";
import { InspectLink } from "./InspectLink";
import { KindChip, KindName } from "./KindMark";
import { doctorOutcomeLamp } from "./doctor";
import { KIND_META } from "./kind";

/**
 * The overview's "Needs attention": every object that needs someone as one
 * row of one shape, the whole row its link — kind and name (opening the resource drawer), a pill
 * saying what is wrong, the problem in the operator's words and the fix on
 * its plate — then the failing doctor checks that are about no object in
 * particular, in the same shape.
 */
export function AttentionList({
  attention,
  namespace,
  now,
}: {
  attention: Attention;
  namespace: string | undefined;
  now: Date;
}) {
  const { items, checks } = attention;
  if (items.length === 0 && checks.length === 0) return null;
  const search = namespace !== undefined ? { namespace } : {};
  return (
    <ul className="attention" aria-label="Objects needing attention">
      {items.map((item) => (
        <ObjectRow key={rowKey(item)} item={item} now={now} />
      ))}
      {checks.map((check) => (
        <li key={check.check} className="attention-row" data-health="failed">
          <span className="attention-row__icon" aria-hidden="true">
            <Stethoscope strokeWidth={2} />
          </span>
          <span className="object-id">
            <span className="kind-name attention-row__check-word">Doctor check</span>
            <Link className="object-name row-link" to="/doctor" search={search}>
              {check.title}
            </Link>
          </span>
          <LampBadge lamp={doctorOutcomeLamp("Fail")} />
          <Problems
            problems={[
              check.fix !== undefined ? { what: check.what, fix: check.fix } : { what: check.what },
            ]}
          />
        </li>
      ))}
    </ul>
  );
}

function rowKey(item: AttentionItem): string {
  const t = item.target;
  return `${t.kind}/${t.namespace ?? ""}/${t.name}`;
}

function ObjectRow({ item, now }: { item: AttentionItem; now: Date }) {
  const { target } = item;
  return (
    <li
      className="attention-row has-stripe"
      data-kind={KIND_META[target.kind].slug}
      data-health={item.health}
    >
      <KindChip kind={target.kind} />
      <span className="object-id">
        <KindName kind={target.kind} />
        <InspectLink className="object-name row-link" target={target}>
          {target.name}
        </InspectLink>
        <span className="object-ns">{target.namespace ?? "cluster-scoped"}</span>
      </span>
      <span className="attention-row__state">
        <HealthBadge health={item.health} label={item.state} />
        {item.at !== undefined ? (
          <time className="attention-row__at" dateTime={item.at}>
            {relativeTime(item.at, now)}
          </time>
        ) : null}
      </span>
      <Problems problems={item.problems} />
    </li>
  );
}

function Problems({ problems }: { problems: readonly AttentionProblem[] }) {
  return (
    <div className="attention-row__body">
      {problems.map((problem) => (
        <div key={problem.what} className="attention-row__problem">
          <p className="attention-row__what">{problem.what}</p>
          {problem.fix !== undefined ? (
            <p className="finding__fix">
              <span className="finding__fix-label">Fix</span>
              <span>{problem.fix}</span>
            </p>
          ) : null}
        </div>
      ))}
    </div>
  );
}
