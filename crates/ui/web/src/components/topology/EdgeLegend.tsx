import { EDGE_LEGEND, edgeStyle } from "./model";

/**
 * What the five lines mean — always on the page, never behind a toggle.
 *
 * A dash pattern is only a vocabulary if the reader can see the dictionary
 * while they read. The legend draws each stroke exactly as the board draws it
 * (same dash, same width, same end marker, from the one `edgeStyle` the board
 * uses), names the relationship as the CRD spells it, and says in one clause
 * what it does — so "the dotted line" is answerable without clicking
 * anything.
 *
 * The samples are drawn in the neutral ink, not in a health colour: the
 * legend is about *kind*. Health lives on the board's own strokes and, in
 * words, on each plate and in the drawer.
 */
export function EdgeLegend() {
  return (
    <ul className="edge-legend">
      {EDGE_LEGEND.map((kind) => {
        const style = edgeStyle(kind);
        return (
          <li className="edge-legend__item" key={style.key} data-edge={style.key}>
            <svg
              className="edge-legend__sample"
              width="56"
              height="12"
              viewBox="0 0 56 12"
              aria-hidden="true"
              focusable="false"
            >
              <line
                x1="1"
                y1="6"
                x2={style.marker === "none" ? 55 : 44}
                y2="6"
                strokeWidth={style.width}
                strokeDasharray={style.dash}
              />
              {style.marker === "arrow" ? <path d="M44,1 L54,6 L44,11 z" /> : null}
              {style.marker === "square" ? <rect x="44" y="2" width="8" height="8" /> : null}
            </svg>
            <span className="edge-legend__word">{style.word}</span>
            <span className="edge-legend__meaning">{style.meaning}</span>
          </li>
        );
      })}
    </ul>
  );
}
