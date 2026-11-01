const marker = /^\s*#\s*(end)?region\b/;

/**
 * The lines between `# region <name>` and `# endregion <name>` in a file imported
 * with `?raw`, minus any nested markers and the blank lines at either end (in
 * the chart's values.yaml a blank line keeps each marker out of the comment
 * helm-schema reads as a key's description). Throws so a renamed or removed
 * region fails the build instead of rendering an empty block.
 */
export function region(source: string, name: string): string {
  const lines = source.split("\n");
  const start = lines.findIndex((line) => line.trim() === `# region ${name}`);
  const end = lines.findIndex((line, i) => i > start && line.trim() === `# endregion ${name}`);
  if (start < 0 || end < 0) {
    throw new Error(`docs: region "${name}" not found`);
  }
  return lines
    .slice(start + 1, end)
    .filter((line) => !marker.test(line))
    .join("\n")
    .replace(/^\n+|\n+$/g, "");
}
