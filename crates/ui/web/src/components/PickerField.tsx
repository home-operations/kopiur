import { ChevronsUpDown } from "lucide-react";
import { useState } from "react";

import { Popover } from "./Popover";
import { type PickerOption, PickerOptions } from "./PickerOptions";

interface CommonProps {
  id: string;
  label: string;
  options: readonly PickerOption[];
  /**
   * What choosing nothing means, shown muted on the button. Given, the list
   * has a Clear button at its top right; a field without it always holds a
   * value.
   */
  emptyLabel?: string | undefined;
  /** What the button says before anything is chosen, for a field with no Clear (no `emptyLabel`). */
  placeholder?: string | undefined;
  /** A filter over the options, naming them ("policies"); a short fixed list has none. */
  search?: string | undefined;
  /** With `search`: told what is typed, so a field can ask the server for more. */
  onSearch?: ((text: string) => void) | undefined;
  /** With `search`: what is typed may be taken when nothing listed matches. */
  freeform?: boolean | undefined;
  /** What the button says for a value no loaded option labels (options not fetched yet). */
  labelFor?: ((value: string) => string) | undefined;
  /** Told when the list is first opened, so a field can fetch its options lazily. */
  onOpen?: (() => void) | undefined;
  /** A line under the field, read as its description. */
  hint?: string | undefined;
  /** For the popover: `fixed` inside a box that clips. */
  strategy?: "absolute" | "fixed" | undefined;
}

/** One value: a pick replaces it and closes the list. */
interface SingleProps extends CommonProps {
  multiple?: false | undefined;
  /** The value chosen, `""` for none. */
  value: string;
  onChange: (value: string, option?: PickerOption) => void;
}

/** Several values: each pick toggles one, and the list stays open. */
interface MultipleProps extends CommonProps {
  multiple: true;
  value: readonly string[];
  onChange: (value: string[]) => void;
}

export type PickerFieldProps = SingleProps | MultipleProps;

/**
 * A form field whose value is picked from a list in a popover, never typed
 * blind and never a native `<select>`: a field-shaped button saying its value,
 * opening `PickerOptions` under it. With `search` the list has a filter field
 * (namespaces, policies, repositories); without, it is the same dropdown for a
 * short fixed list (a mode, a phase). `multiple` makes every pick a toggle;
 * choosing nothing is the Clear button, never an "any" row.
 *
 * Controlled: the form owns the value and decides when it is applied.
 */
export function PickerField(props: PickerFieldProps) {
  const {
    id,
    label,
    options,
    emptyLabel,
    placeholder,
    search,
    onSearch,
    freeform = false,
    labelFor,
    onOpen,
    hint,
    strategy,
  } = props;
  const [open, setOpen] = useState(false);
  const selected: readonly string[] = props.multiple
    ? props.value
    : props.value.length > 0
      ? [props.value]
      : [];
  const empty = selected.length === 0;
  const hintId = `${id}-hint`;
  const name = (value: string) =>
    options.find((option) => option.value === value)?.label ?? labelFor?.(value) ?? value;
  const names = selected.map(name);
  const none = emptyLabel ?? placeholder ?? "";
  const first = names[0];
  const shown =
    first === undefined
      ? none
      : names.length === 1
        ? first
        : `${first} +${String(names.length - 1)}`;

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
        initialFocus={
          search !== undefined
            ? "input[type=search]"
            : '.picker__option[aria-current="true"], .picker__option[aria-pressed="true"], .picker__option'
        }
        trigger={(trigger) => (
          <button
            type="button"
            id={id}
            className="controls__input picker-field__button"
            data-empty={empty ? "true" : undefined}
            aria-describedby={hint !== undefined ? hintId : undefined}
            {...trigger}
            aria-label={`${label}: ${empty ? none : names.join(", ")}`}
          >
            <span className="picker-field__value">{shown}</span>
            <ChevronsUpDown size={14} strokeWidth={2} aria-hidden="true" />
          </button>
        )}
      >
        {(close) => (
          <PickerOptions
            options={options}
            selected={selected}
            multiple={props.multiple}
            search={search !== undefined ? { noun: search, onChange: onSearch } : undefined}
            freeform={freeform}
            onClear={
              emptyLabel === undefined
                ? undefined
                : () => {
                    if (props.multiple) {
                      props.onChange([]);
                    } else {
                      props.onChange("");
                      close();
                    }
                  }
            }
            onChoose={(value, option) => {
              if (props.multiple) {
                props.onChange(
                  props.value.includes(value)
                    ? props.value.filter((v) => v !== value)
                    : [...props.value, value],
                );
              } else {
                props.onChange(value, option);
                close();
              }
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
