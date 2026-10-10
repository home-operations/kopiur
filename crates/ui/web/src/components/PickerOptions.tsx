import { Check, Search } from "lucide-react";
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
  /** The value chosen now; `undefined` is the empty choice. */
  current: string | undefined;
  /** Called with the value picked, or `undefined` for the empty choice. */
  onChoose: (value: string | undefined, option?: PickerOption) => void;
  /** The empty choice, listed first; absent when the list has none. */
  empty?: { label: string; meta?: ReactNode; initial?: string | undefined } | undefined;
  /** A filter field over the list, with what it filters ("namespaces"). */
  search?: { noun: string } | undefined;
  /** With `search`: offer what was typed when nothing listed has that value. */
  freeform?: boolean | undefined;
}

const OPTION = ".picker__option";

/**
 * The body of every picker: an optional filter field over a list of choices,
 * the empty choice first. The namespace switcher, a form's namespace, policy
 * and repository fields, and every short fixed list (a mode, a phase) open
 * this same list, so a choice looks and behaves one way across the console.
 *
 * Arrow keys move between the choices (down from the filter enters the list);
 * Enter in the filter takes the one match, or the typed text where allowed,
 * and never submits a surrounding form.
 */
export function PickerOptions({
  options,
  current,
  onChoose,
  empty,
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
        aria-current={chosen ? "true" : undefined}
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

  return (
    <div ref={root} className="picker">
      {search !== undefined ? (
        <label className="picker__filter">
          <Search size={14} strokeWidth={2} aria-hidden="true" />
          <input
            type="search"
            aria-label={`Filter ${search.noun}`}
            placeholder={freeform ? `Filter or type a name…` : `Filter ${search.noun}…`}
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
              if (only !== undefined) onChoose(only.value, only);
              else if (offerTyped) onChoose(typed);
            }}
          />
        </label>
      ) : null}
      <ul className="picker__list">
        {empty !== undefined
          ? row(
              "\u0000empty",
              empty.label,
              () => {
                onChoose(undefined);
              },
              current === undefined,
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
                onChoose(typed);
              },
              false,
              null,
              "not listed",
            )
          : null}
        {shown.map((option) =>
          row(
            option.value,
            option.label,
            () => {
              onChoose(option.value, option);
            },
            current === option.value,
            avatar(option),
            option.meta,
          ),
        )}
      </ul>
    </div>
  );
}
