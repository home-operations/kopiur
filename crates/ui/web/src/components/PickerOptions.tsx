import { Check, Search, X } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";

/** One choice in a picker. */
export interface PickerOption {
  /** What choosing it sets; unique within the list. */
  value: string;
  /** What it reads as. */
  label: string;
  /** A mark before the label; with neither this nor `initial`, none. */
  icon?: ReactNode;
  /** A letter avatar before the label (namespaces). */
  initial?: string | undefined;
  /** Faint text after the label: a count, a namespace. */
  meta?: ReactNode;
}

export interface PickerOptionsProps {
  options: readonly PickerOption[];
  /** The values chosen now; empty is nothing chosen. */
  selected: readonly string[];
  /**
   * Called with the value picked (a typed value has no option). In a
   * `multiple` list a pick toggles that value; otherwise it replaces the one.
   */
  onChoose: (value: string, option?: PickerOption) => void;
  /** Several values may be chosen: each pick toggles, and the list stays open. */
  multiple?: boolean | undefined;
  /** Empty the choice; given, a Clear button sits at the top right. */
  onClear?: (() => void) | undefined;
  /**
   * An explicit empty choice listed first — only for a scope that is itself a
   * choice ("all namespaces", with its count). A filter clears instead.
   */
  empty?: { label: string; meta?: ReactNode; initial?: string | undefined } | undefined;
  /** For `empty`: chosen now. */
  emptyChosen?: boolean | undefined;
  /** For `empty`: picked. */
  onEmpty?: (() => void) | undefined;
  /** A filter field over the list, with what it filters ("namespaces"). */
  search?: { noun: string } | undefined;
  /** With `search`: offer what was typed when nothing listed has that value. */
  freeform?: boolean | undefined;
}

const OPTION = ".picker__option";

/**
 * The body of every picker: a filter field (optional) and a Clear button
 * over a list of choices. The namespace switcher, every filter, and every
 * short fixed list in a form (a mode, a kind) open this same list, so a
 * choice looks and behaves one way across the console.
 *
 * Arrow keys move between the choices (down from the filter enters the list);
 * Enter in the filter takes the one match, or the typed text where allowed,
 * and never submits a surrounding form.
 */
export function PickerOptions({
  options,
  selected,
  onChoose,
  multiple = false,
  onClear,
  empty,
  emptyChosen = false,
  onEmpty,
  search,
  freeform = false,
}: PickerOptionsProps) {
  const [filter, setFilter] = useState("");
  const needle = filter.trim().toLowerCase();
  const shown =
    search === undefined
      ? options
      : options.filter((option) => option.label.toLowerCase().includes(needle));
  const typed = filter.trim();
  const offerTyped =
    search !== undefined &&
    freeform &&
    typed.length > 0 &&
    !options.some((option) => option.value === typed || option.label === typed);
  // A typed value already chosen is listed like an option, so it can be unticked.
  const chosenUnlisted = selected.filter((value) => !options.some((o) => o.value === value));

  // Arrow keys walk the choices, heard on the whole picker so they work
  // from the filter field too.
  const root = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    const node = root.current;
    if (node === null) return undefined;
    const step = (event: KeyboardEvent) => {
      const by = event.key === "ArrowDown" ? 1 : event.key === "ArrowUp" ? -1 : 0;
      if (by === 0) return;
      const buttons = Array.from(node.querySelectorAll<HTMLButtonElement>(OPTION));
      if (buttons.length === 0) return;
      event.preventDefault();
      const at = buttons.indexOf(document.activeElement as HTMLButtonElement);
      const next = at === -1 ? (by > 0 ? 0 : buttons.length - 1) : at + by;
      buttons[Math.max(0, Math.min(buttons.length - 1, next))]?.focus();
    };
    node.addEventListener("keydown", step);
    return () => {
      node.removeEventListener("keydown", step);
    };
  }, []);

  const pick = (value: string, option?: PickerOption) => {
    onChoose(value, option);
    if (multiple) setFilter("");
  };

  const row = (
    key: string,
    label: string,
    choose: () => void,
    chosen: boolean,
    mark: ReactNode,
    meta: ReactNode,
  ) => (
    <li key={key}>
      <button
        type="button"
        className="picker__option"
        aria-pressed={multiple ? chosen : undefined}
        aria-current={!multiple && chosen ? "true" : undefined}
        onClick={choose}
      >
        {mark}
        <span className="picker__label">{label}</span>
        {meta !== undefined && meta !== null ? <span className="picker__meta">{meta}</span> : null}
        {chosen ? <Check size={14} strokeWidth={2} aria-hidden="true" /> : null}
      </button>
    </li>
  );

  const avatar = (option: PickerOption): ReactNode =>
    option.icon ??
    (option.initial !== undefined ? (
      <span className="picker__avatar" data-initial={option.initial} aria-hidden="true" />
    ) : null);

  const clear =
    onClear !== undefined ? (
      <button
        type="button"
        className="picker__clear"
        disabled={selected.length === 0}
        onClick={() => {
          onClear();
          // The button just disabled itself; focus moves on into the list.
          root.current?.querySelector<HTMLElement>(`input[type=search], ${OPTION}`)?.focus();
        }}
      >
        <X size={12} strokeWidth={2} aria-hidden="true" />
        Clear
      </button>
    ) : null;

  return (
    <div ref={root} className="picker">
      {search !== undefined || clear !== null ? (
        <div className="picker__head">
          {search !== undefined ? (
            <label className="picker__filter">
              <Search size={14} strokeWidth={2} aria-hidden="true" />
              <input
                type="search"
                aria-label={`Filter ${search.noun}`}
                placeholder={freeform ? "Filter or type a name…" : `Filter ${search.noun}…`}
                value={filter}
                autoComplete="off"
                spellCheck={false}
                onChange={(event) => {
                  setFilter(event.target.value);
                }}
                onKeyDown={(event) => {
                  if (event.key !== "Enter") return;
                  event.preventDefault();
                  const only = shown.length === 1 ? shown[0] : undefined;
                  if (only !== undefined) pick(only.value, only);
                  else if (offerTyped) pick(typed);
                }}
              />
            </label>
          ) : (
            <span className="picker__summary">
              {selected.length === 0 ? "none chosen" : `${String(selected.length)} chosen`}
            </span>
          )}
          {clear}
        </div>
      ) : null}
      <ul className="picker__list">
        {empty !== undefined
          ? row(
              "\u0000empty",
              empty.label,
              () => {
                onEmpty?.();
              },
              emptyChosen,
              empty.initial !== undefined ? (
                <span
                  className="picker__avatar picker__avatar--all"
                  data-initial={empty.initial}
                  aria-hidden="true"
                />
              ) : null,
              empty.meta,
            )
          : null}
        {offerTyped
          ? row(
              "\u0000typed",
              typed,
              () => {
                pick(typed);
              },
              false,
              null,
              "not listed",
            )
          : null}
        {chosenUnlisted.map((value) =>
          row(
            `\u0000chosen-${value}`,
            value,
            () => {
              pick(value);
            },
            true,
            null,
            "not listed",
          ),
        )}
        {shown.map((option) =>
          row(
            option.value,
            option.label,
            () => {
              pick(option.value, option);
            },
            selected.includes(option.value),
            avatar(option),
            option.meta,
          ),
        )}
      </ul>
    </div>
  );
}
