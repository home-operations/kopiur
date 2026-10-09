import type { ActivityItem } from "./activity";
import { relativeTime } from "../util/format";
import { LampBadge } from "./HealthBadge";
import { InspectLink } from "./InspectLink";
import { KindChip, KindName } from "./KindMark";
import { KIND_META } from "./kind";

/**
 * The overview's "Recent activity": one compact row per run, newest first,
 * the whole row opening the object — the attention row's shape, smaller, so
 * the two columns read alike.
 */
export function ActivityList({ items, now }: { items: readonly ActivityItem[]; now: Date }) {
  return (
    <ul className="activity" aria-label="Recent runs">
      {items.map((item) => {
        const { target } = item;
        return (
          <li
            key={`${target.kind}/${target.namespace ?? ""}/${target.name}/${item.what}/${item.at}`}
            className="activity-row"
            data-kind={KIND_META[target.kind].slug}
          >
            <KindChip kind={target.kind} size="sm" />
            <span className="object-id">
              <KindName kind={target.kind} />
              <InspectLink className="object-name row-link" target={target}>
                {target.name}
              </InspectLink>
              <span className="object-ns">{target.namespace ?? "cluster-scoped"}</span>
            </span>
            <span className="activity-row__state">
              <LampBadge lamp={item.lamp} />
              <time className="attention-row__at" dateTime={item.at}>
                {relativeTime(item.at, now)}
              </time>
            </span>
            {item.what.length > 0 ? <p className="activity-row__what">{item.what}</p> : null}
          </li>
        );
      })}
    </ul>
  );
}
