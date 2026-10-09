import { healthLamp } from "../health";
import type { NavTally } from "../navTally";

/**
 * A section's count and health, drawn: a failed pill when anything is
 * failing (so the state is never colour alone), the total, and a thin bar of
 * the health buckets under the name. All of it is decoration over the link's
 * description, which says the same in words — so none of it is spoken.
 */
export function NavTallyMarks({ tally }: { tally: NavTally }) {
  const failed = healthLamp("failed");
  const Icon = failed.icon;
  return (
    <>
      <span className="nav-item__tally" aria-hidden="true">
        {tally.failing > 0 ? (
          <span className="health health--compact" data-health="failed">
            <Icon size={11} strokeWidth={2.25} />
            <span>{tally.failing}</span>
          </span>
        ) : null}
        <span className="nav-item__count">{tally.total}</span>
      </span>
      <span className="status-bar status-bar--nav" aria-hidden="true">
        {tally.parts.map((p) => (
          <span key={p.key} data-health={p.key} style={{ flexGrow: p.n }} />
        ))}
      </span>
    </>
  );
}
