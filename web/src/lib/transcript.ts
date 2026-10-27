/**
 * Where a line index (the edited or cursor line) points after the line at
 * `deleted` is removed: later lines move up by one, the deleted line itself
 * has no successor and becomes `null`.
 */
export function shiftAfterDelete(index: number | null, deleted: number): number | null {
  if (index === null || index === deleted) return null;
  return index > deleted ? index - 1 : index;
}
