import { Monitor, Moon, Sun, type LucideIcon } from "lucide-react";

import { type ThemePreference, useThemePreference } from "../../util/theme";

const THEME_OPTIONS: readonly { value: ThemePreference; label: string; icon: LucideIcon }[] = [
  { value: "system", label: "System", icon: Monitor },
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
];

/** System / Light / Dark. Each button is named by its label, whatever is visible. */
export function ThemeSwitch() {
  const [preference, setPreference] = useThemePreference();
  return (
    <div className="theme-switch" role="group" aria-label="Theme">
      {THEME_OPTIONS.map((option) => {
        const Icon = option.icon;
        return (
          <button
            key={option.value}
            type="button"
            className="theme-switch__option"
            aria-label={option.label}
            aria-pressed={preference === option.value}
            onClick={() => {
              setPreference(option.value);
            }}
          >
            <Icon size={14} strokeWidth={1.75} aria-hidden="true" />
            <span>{option.label}</span>
          </button>
        );
      })}
    </div>
  );
}
