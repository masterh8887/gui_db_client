// Esc precedence helpers (DESIGN.md §6 "Esc precedence"): a component whose Esc does something
// local (a search field with text, the tab rename input, anything marked `data-esc-local`) keeps
// it from closing the surrounding dialog / sheet / popover.

/** Whether an Escape keydown belongs to the focused control rather than its container. */
export function escapeIsLocal(e: Event): boolean {
  const target = e.target;
  if (!(target instanceof HTMLElement)) return false;
  if (target.closest('[data-esc-local]')) return true;
  if (target instanceof HTMLInputElement && target.type === 'search' && target.value.length > 0) return true;
  return target.closest('.cm-tooltip-autocomplete') !== null;
}
