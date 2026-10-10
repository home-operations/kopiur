import { ChevronsUpDown } from "lucide-react";
import { useState } from "react";

import { NamespaceOptions } from "./NamespaceOptions";
import { Popover } from "./Popover";

export interface NamespaceFieldProps {
  id: string;
  label: string;
  /** The namespace named, `""` for none. */
  value: string;
  onChange: (value: string) => void;
  /** What leaving it empty means, shown on the button and as the list's first choice. */
  emptyLabel: string;
  /** For the popover: `fixed` inside a box that clips. */
  strategy?: "absolute" | "fixed" | undefined;
  /** A line under the field, read as its description. */
  hint?: string | undefined;
}

/**
 * A form's namespace, picked from the same list as the shell's namespace
 * switcher rather than typed blind: the namespaces the caller may see, with
 * their counts, behind a filter. A namespace the caller cannot list can still
 * be typed into the filter and taken as it is.
 *
 * Controlled, like the text field it replaces: the form owns the value and
 * decides when it is applied.
 */
export function NamespaceField({
  id,
  label,
  value,
  onChange,
  emptyLabel,
  strategy,
  hint,
}: NamespaceFieldProps) {
  const [open, setOpen] = useState(false);
  const empty = value.length === 0;
  const hintId = `${id}-hint`;
  return (
    <div className="controls__field namespace-field">
      <label htmlFor={id}>{label}</label>
      <Popover
        label={`Choose the ${label.toLowerCase()}`}
        open={open}
        onOpenChange={setOpen}
        strategy={strategy}
        className="ns-switcher__panel namespace-field__panel"
        initialFocus="input[type=search]"
        trigger={(props) => (
          <button
            type="button"
            id={id}
            className="controls__input namespace-field__button"
            data-empty={empty ? "true" : undefined}
            aria-describedby={hint !== undefined ? hintId : undefined}
            {...props}
            aria-label={`${label}: ${empty ? emptyLabel : value}`}
          >
            <span className="namespace-field__value">{empty ? emptyLabel : value}</span>
            <ChevronsUpDown size={14} strokeWidth={2} aria-hidden="true" />
          </button>
        )}
      >
        {(close) => (
          <NamespaceOptions
            current={empty ? undefined : value}
            emptyLabel={emptyLabel}
            freeform
            onChoose={(namespace) => {
              onChange(namespace ?? "");
              close();
            }}
          />
        )}
      </Popover>
      {hint !== undefined ? (
        <span className="controls__hint" id={hintId}>
          {hint}
        </span>
      ) : null}
    </div>
  );
}
