import { ChevronsUpDown } from "lucide-react";
import { useState } from "react";

import { Popover } from "./Popover";
import { type PickerOption, PickerOptions } from "./PickerOptions";

export interface PickerFieldProps {
  id: string;
  label: string;
  /** The value chosen, `""` for none. */
  value: string;
  onChange: (value: string, option?: PickerOption) => void;
  options: readonly PickerOption[];
  /**
   * What leaving it empty means — shown on the button and listed first. A
   * field without it always holds one of `options`.
   */
  emptyLabel?: string | undefined;
  /** The empty choice's avatar letter, when the options carry avatars. */
  emptyInitial?: string | undefined;
  /** A filter over the options, naming them ("policies"); a short fixed list has none. */
  search?: string | undefined;
  /** With `search`: what is typed may be taken when nothing listed matches. */
  freeform?: boolean | undefined;
  /** What the button says for `value`, when no option's label does (options not loaded yet). */
  valueLabel?: string | undefined;
  /** Told when the list is first opened, so a field can fetch its options lazily. */
  onOpen?: (() => void) | undefined;
  /** A line under the field, read as its description. */
  hint?: string | undefined;
  /** For the popover: `fixed` inside a box that clips. */
  strategy?: "absolute" | "fixed" | undefined;
}

/**
 * A form field whose value is picked from a list in a popover, never typed
 * blind and never a native `<select>`: a field-shaped button saying its value,
 * opening `PickerOptions` under it. With `search` the list has a filter field
 * (namespaces, policies, repositories); without, it is the same dropdown for a
 * short fixed list (a mode, a phase).
 *
 * Controlled: the form owns the value and decides when it is applied.
 */
export function PickerField({
  id,
  label,
  value,
  onChange,
  options,
  emptyLabel,
  emptyInitial,
  search,
  freeform = false,
  valueLabel,
  onOpen,
  hint,
  strategy,
}: PickerFieldProps) {
  const [open, setOpen] = useState(false);
  const empty = value.length === 0;
  const hintId = `${id}-hint`;
  const shown = empty
    ? (emptyLabel ?? "")
    : (options.find((option) => option.value === value)?.label ?? valueLabel ?? value);
  return (
    <div className="controls__field picker-field">
      <label htmlFor={id}>{label}</label>
      <Popover
        label={`Choose the ${label.toLowerCase()}`}
        open={open}
        onOpenChange={(next) => {
          if (next) onOpen?.();
          setOpen(next);
        }}
        strategy={strategy}
        className="picker-field__panel"
        initialFocus={search !== undefined ? "input[type=search]" : '[aria-current="true"]'}
        trigger={(props) => (
          <button
            type="button"
            id={id}
            className="controls__input picker-field__button"
            data-empty={empty ? "true" : undefined}
            aria-describedby={hint !== undefined ? hintId : undefined}
            {...props}
            aria-label={`${label}: ${shown}`}
          >
            <span className="picker-field__value">{shown}</span>
            <ChevronsUpDown size={14} strokeWidth={2} aria-hidden="true" />
          </button>
        )}
      >
        {(close) => (
          <PickerOptions
            options={options}
            current={empty ? undefined : value}
            empty={
              emptyLabel !== undefined ? { label: emptyLabel, initial: emptyInitial } : undefined
            }
            search={search !== undefined ? { noun: search } : undefined}
            freeform={freeform}
            onChoose={(next, option) => {
              onChange(next ?? "", option);
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
