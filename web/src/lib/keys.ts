/** True when a key event should be left to a text field or a browser shortcut. */
export function ignoreKey(e: KeyboardEvent): boolean {
  if (e.metaKey || e.ctrlKey || e.altKey || e.defaultPrevented) return true;
  const el = e.target as HTMLElement | null;
  if (!el) return false;
  return (
    el.isContentEditable ||
    el.tagName === 'INPUT' ||
    el.tagName === 'TEXTAREA' ||
    el.tagName === 'SELECT'
  );
}
