import { Link } from "@tanstack/react-router";
import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";

import type { GateHit } from "../../api/types";
import { relativeTime } from "../../util/format";
import { Finding } from "../Finding";
import { gateSeverityLamp } from "../gates";

/** One named region of a drawer tab: an icon, a small heading, its content. */
export function DrawerSection({
  title,
  icon: Icon,
  children,
}: {
  title: string;
  icon: LucideIcon;
  children: ReactNode;
}) {
  return (
    <section className="page__section drawer__region" aria-label={title}>
      <h3 className="drawer__region-title">
        <Icon size={14} strokeWidth={2} aria-hidden="true" />
        {title}
      </h3>
      {children}
    </section>
  );
}

/** An instant with a direction ("3h ago"), or the empty cell. */
export function Instant({ at, now }: { at: string | null | undefined; now: Date }) {
  if (at === null || at === undefined || at.length === 0) return <>-</>;
  return <time dateTime={at}>{relativeTime(at, now)}</time>;
}

/**
 * The gates holding a resource, each as what / why it matters, with the
 * registry one link away. A gate never self-heals, so this rides at the top of
 * the drawer, not in a tab.
 */
export function GateFindings({
  gates,
  title,
  namespace,
}: {
  gates: readonly GateHit[];
  /** "Gates holding this repository". */
  title: string;
  namespace?: string | undefined;
}) {
  if (gates.length === 0) return null;
  return (
    <section className="drawer__region" aria-label={title}>
      <ul className="finding-list">
        {gates.map((gate) => {
          const lamp = gateSeverityLamp(gate.severity);
          return (
            <li key={`${gate.condition}/${gate.reason}`}>
              <Finding
                title={gate.reason}
                what={gate.message}
                lamp={lamp}
                meta={
                  <span className="mono">
                    condition {gate.condition} · {lamp.word}
                  </span>
                }
              />
            </li>
          );
        })}
      </ul>
      <p className="page__section-note">
        A gate never self-heals — it holds until a human acts. The{" "}
        <Link to="/gates" search={namespace !== undefined ? { namespace } : {}}>
          gate registry
        </Link>{" "}
        explains every gate the operator can raise.
      </p>
    </section>
  );
}
