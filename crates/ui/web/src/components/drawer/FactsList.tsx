import { AbsenceText } from "../StatStrip";
import type { DrawerFact } from "./drawerFacts";

/** Labelled values, label left and value right; a value you would copy is set in mono. */
export function FactsList({
  facts,
  label = "Facts",
}: {
  facts: readonly DrawerFact[];
  label?: string;
}) {
  return (
    <section className="drawer__section" aria-label={label}>
      <dl className="drawer__facts">
        {facts.map((fact) => (
          <div className="drawer__fact" key={fact.label}>
            <dt>{fact.label}</dt>
            <dd className={fact.mono === true ? "mono" : undefined}>
              {typeof fact.value === "string" ? fact.value : <AbsenceText absence={fact.value} />}
            </dd>
          </div>
        ))}
      </dl>
    </section>
  );
}
