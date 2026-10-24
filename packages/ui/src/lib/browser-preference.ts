import { useState } from "react";

export function useBrowserPreference<
  const T extends readonly [string, ...string[]],
>(key: string, choices: T) {
  const [value, setValue] = useState<T[number]>(() => {
    try {
      const stored = localStorage.getItem(key);
      if (stored !== null && choices.includes(stored)) return stored;
    } catch {
      // Browser policy can deny storage. Optional presentation preferences must
      // not disable navigation; the current selection still works in memory.
    }
    return choices[0];
  });
  function update(next: T[number]) {
    setValue(next);
    try {
      localStorage.setItem(key, next);
    } catch {
      // Quota or storage policy can prevent persistence, not the interaction.
    }
  }
  return [value, update] as const;
}
