// Completion popup for the SQL editor.
//
// `@codemirror/autocomplete` is not a direct dependency of this app (only state / view /
// language / commands / lang-sql are), so this module provides the small part of it the editor
// needs: it asks the completion sources that `@codemirror/lang-sql` (and the editor) register as
// `autocomplete` language data, ranks their options against the word before the cursor and shows
// them in a CodeMirror tooltip. Sources are called with a context object that implements the
// members those sources use (`state`, `pos`, `explicit`, `matchBefore`, `tokenBefore`).
//
// Keys while the list is open: ArrowUp / ArrowDown / PageUp / PageDown move, Enter / Tab accept,
// Escape closes. Ctrl-Space (also Alt-` on macOS) opens it explicitly. Typing an identifier
// character opens it.

import { syntaxTree } from '@codemirror/language';
import type { schemaCompletionSource } from '@codemirror/lang-sql';
import { Prec, StateEffect, StateField, type EditorState, type Extension } from '@codemirror/state';
import { EditorView, keymap, showTooltip, type Command, type Tooltip, type TooltipView, type ViewUpdate } from '@codemirror/view';

/** A completion source as `@codemirror/lang-sql` defines it (`CompletionSource`). */
export type SqlCompletionSource = ReturnType<typeof schemaCompletionSource>;
/** The context a source receives (`CompletionContext`). */
export type SqlCompletionContext = Parameters<SqlCompletionSource>[0];
/** What a source returns (`CompletionResult`). */
export type SqlCompletionResult = NonNullable<Awaited<ReturnType<SqlCompletionSource>>>;
/** One option (`Completion`). */
export type SqlCompletion = SqlCompletionResult['options'][number];

/** The open completion list. Positions refer to the state the value belongs to. */
export interface ActiveCompletion {
  /** Start of the text the accepted option replaces. */
  from: number;
  /** End of that text (the cursor, or past a closing quote). */
  to: number;
  /** Ranked options, best first, at most `MAX_OPTIONS`. */
  options: readonly SqlCompletion[];
  selected: number;
  /** Opened with Ctrl-Space (keeps showing options for an empty prefix while typing). */
  explicit: boolean;
}

/** Options shown at most; the rest are reachable by typing more of the name. */
export const MAX_OPTIONS = 200;
/** How many rows PageUp / PageDown move. */
const PAGE = 8;
/** `matchBefore` looks at most this far back on the cursor line (same as @codemirror/autocomplete). */
const MATCH_WINDOW = 250;

const startEffect = StateEffect.define<null>();
const closeEffect = StateEffect.define<null>();
const selectEffect = StateEffect.define<number>();

// ---- source context ------------------------------------------------------------------------

function anchored(expr: RegExp): RegExp {
  return expr.source.endsWith('$') ? expr : new RegExp(`(?:${expr.source})$`, expr.flags);
}

/**
 * Build the context passed to completion sources. It mirrors `CompletionContext` from
 * `@codemirror/autocomplete` for the members sources read; nothing is ever aborted because every
 * source here is synchronous.
 */
export function completionContext(state: EditorState, pos: number, explicit: boolean): SqlCompletionContext {
  const ctx = {
    state,
    pos,
    explicit,
    view: undefined,
    aborted: false,
    addEventListener: () => undefined,
    matchBefore(expr: RegExp) {
      const line = state.doc.lineAt(pos);
      const start = Math.max(line.from, pos - MATCH_WINDOW);
      const str = line.text.slice(start - line.from, pos - line.from);
      const found = str.search(anchored(expr));
      return found < 0 ? null : { from: start + found, to: pos, text: str.slice(found) };
    },
    tokenBefore(types: readonly string[]) {
      let node: ReturnType<ReturnType<typeof syntaxTree>['resolveInner']> | null = syntaxTree(state).resolveInner(pos, -1);
      while (node && !types.includes(node.name)) node = node.parent;
      return node ? { from: node.from, to: pos, text: state.sliceDoc(node.from, pos), type: node.type } : null;
    },
  };
  // The real class has private members, so a structural object needs this cast.
  return ctx as unknown as SqlCompletionContext;
}

// ---- ranking -------------------------------------------------------------------------------

const QUOTES = /^[`"'[]|[`"'\]]$/g;

function bare(s: string): string {
  return s.replace(QUOTES, '').toLowerCase();
}

/**
 * Filter and order `options` for the typed prefix: exact match, then prefix, then a match at a
 * word boundary inside the name (`ord` finds `customer_orders`), then (for two or more typed
 * characters) any substring. `boost` breaks ties inside a class; shorter labels come first.
 * Duplicates (same label and type) are dropped.
 */
export function rankCompletions(options: readonly SqlCompletion[], typed: string): SqlCompletion[] {
  const needle = bare(typed);
  const seen = new Set<string>();
  const scored: { c: SqlCompletion; score: number }[] = [];
  for (const c of options) {
    const key = `${c.label}\u0000${c.type ?? ''}`;
    if (seen.has(key)) continue;
    seen.add(key);
    const label = bare(c.displayLabel ?? c.label);
    let score: number;
    if (!needle) score = 1;
    else if (label === needle) score = 5;
    else if (label.startsWith(needle)) score = 4;
    else {
      const at = label.indexOf(needle);
      if (at < 0) continue;
      if (/[^a-z0-9]/.test(label[at - 1] ?? '')) score = 3;
      else if (needle.length >= 2) score = 2;
      else continue;
    }
    scored.push({ c, score: score + Math.max(-99, Math.min(99, c.boost ?? 0)) / 100 });
  }
  scored.sort((a, b) => b.score - a.score || a.c.label.length - b.c.label.length || a.c.label.localeCompare(b.c.label));
  return scored.map((s) => s.c);
}

function isPromise(v: unknown): v is PromiseLike<unknown> {
  return typeof v === 'object' && v !== null && typeof (v as { then?: unknown }).then === 'function';
}

/**
 * Ask every `autocomplete` source registered at the cursor and build the list to show, or
 * `null` when there is nothing to offer. Only the results that start closest to the cursor are
 * merged (after `schema.` the table names win over keyword matches of the whole word).
 */
export function computeCompletions(state: EditorState, explicit: boolean): ActiveCompletion | null {
  if (state.readOnly) return null;
  const sel = state.selection.main;
  if (!sel.empty) return null;
  const pos = sel.head;
  const ctx = completionContext(state, pos, explicit);
  const results: SqlCompletionResult[] = [];
  for (const source of state.languageDataAt<unknown>('autocomplete', pos, -1)) {
    if (typeof source !== 'function') continue;
    let result: ReturnType<SqlCompletionSource>;
    try {
      result = (source as SqlCompletionSource)(ctx);
    } catch {
      // A failing source must never break typing; the others still complete.
      continue;
    }
    if (result && !isPromise(result) && result.options.length > 0) results.push(result);
  }
  if (results.length === 0) return null;
  const from = Math.max(...results.map((r) => r.from));
  const chosen = results.filter((r) => r.from === from);
  const to = Math.max(pos, ...chosen.map((r) => r.to ?? pos));
  const typed = state.sliceDoc(from, pos);
  // After `qualifier.` only names make sense, not keywords.
  const qualified = state.sliceDoc(from - 1, from) === '.';
  const ranked = rankCompletions(
    chosen.flatMap((r) => (qualified ? r.options.filter((o) => o.type !== 'keyword') : r.options)),
    typed,
  );
  if (ranked.length === 0) return null;
  // Nothing left to complete: the only candidate is exactly what was typed.
  if (!explicit && ranked.length === 1 && ranked[0].label === typed) return null;
  return { from, to, options: ranked.slice(0, MAX_OPTIONS), selected: 0, explicit };
}

// ---- state ---------------------------------------------------------------------------------

/** The open list, or `null`. Exported for tests and for editors that want to inspect it. */
export const completionState = StateField.define<ActiveCompletion | null>({
  create: () => null,
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(closeEffect)) return null;
      if (e.is(startEffect)) return computeCompletions(tr.state, true);
      if (e.is(selectEffect) && value) return { ...value, selected: e.value };
    }
    if (tr.docChanged) {
      // Typing (re)opens the list; backspace only refreshes an open one. Anything else (undo,
      // paste, an external value, an accepted option) closes it.
      if (tr.isUserEvent('input.type') || (value && tr.isUserEvent('delete.backward'))) {
        // An explicitly opened list stays explicit only while the same word is being typed.
        const head = tr.state.selection.main.head;
        const explicit = !!value?.explicit && /[\w$]/.test(tr.state.sliceDoc(head - 1, head));
        return computeCompletions(tr.state, explicit);
      }
      return null;
    }
    if (value && tr.selection) return null;
    return value;
  },
  provide: (field) => showTooltip.from(field, (value): Tooltip | null => (value ? { pos: value.from, above: false, create: createListView } : null)),
});

/** One id per editor for `aria-controls` / `aria-activedescendant`. */
let listSeq = 0;
const listId = StateField.define<string>({
  create: () => `cm-sql-completions-${++listSeq}`,
  update: (v) => v,
});

// ---- commands ------------------------------------------------------------------------------

/** Open the list for the word before the cursor (Ctrl-Space). */
export const startSqlCompletion: Command = (view) => {
  if (view.state.readOnly) return false;
  view.dispatch({ effects: startEffect.of(null) });
  return true;
};

/** Close the list; `false` (key not handled) when it is not open. */
export const closeSqlCompletion: Command = (view) => {
  if (!view.state.field(completionState, false)) return false;
  view.dispatch({ effects: closeEffect.of(null) });
  return true;
};

function moveBy(step: number, wrap: boolean): Command {
  return (view) => {
    const active = view.state.field(completionState, false);
    if (!active) return false;
    const n = active.options.length;
    let next = active.selected + step;
    if (wrap) next = ((next % n) + n) % n;
    else next = Math.max(0, Math.min(n - 1, next));
    if (next !== active.selected) view.dispatch({ effects: selectEffect.of(next) });
    return true;
  };
}

/**
 * Insert option `index` (default: the selected one) over the typed prefix. Returns `false`
 * when no list is open, so Enter / Tab keep their normal meaning.
 */
export function acceptSqlCompletion(view: EditorView, index?: number): boolean {
  const active = view.state.field(completionState, false);
  if (!active || view.state.readOnly) return false;
  const option = active.options[index ?? active.selected];
  if (!option) return false;
  if (typeof option.apply === 'function') {
    view.dispatch({ effects: closeEffect.of(null) });
    option.apply(view, option, active.from, active.to);
    return true;
  }
  const text = option.apply ?? option.label;
  view.dispatch({
    changes: { from: active.from, to: active.to, insert: text },
    selection: { anchor: active.from + text.length },
    userEvent: 'input.complete',
    scrollIntoView: true,
    effects: closeEffect.of(null),
  });
  return true;
}

// ---- view ----------------------------------------------------------------------------------

const TYPE_ICON: Record<string, string> = {
  keyword: 'K',
  type: 'T',
  namespace: 'S',
  property: 'C',
  variable: 'V',
  constant: 'A',
  function: 'F',
};

const TYPE_TITLE: Record<string, string> = {
  keyword: 'keyword',
  type: 'table',
  namespace: 'schema',
  property: 'column',
  variable: 'name',
  constant: 'alias',
  function: 'function',
};

class CompletionListView implements TooltipView {
  readonly dom: HTMLElement;
  private readonly list: HTMLUListElement;
  private readonly id: string;
  private current: ActiveCompletion | null = null;

  constructor(private readonly view: EditorView) {
    const doc = view.dom.ownerDocument;
    this.id = view.state.field(listId);
    this.dom = doc.createElement('div');
    this.dom.className = 'cm-sql-completions';
    this.list = doc.createElement('ul');
    this.list.id = this.id;
    this.list.setAttribute('role', 'listbox');
    this.list.setAttribute('aria-label', 'SQL completions');
    this.dom.appendChild(this.list);
    // `mousedown` + preventDefault keeps focus (and the list) in the editor.
    this.dom.addEventListener('mousedown', (e) => {
      const item = (e.target as Element | null)?.closest?.('[data-index]') as HTMLElement | null;
      if (!item) return;
      e.preventDefault();
      acceptSqlCompletion(this.view, Number(item.dataset.index));
    });
    this.render(view.state.field(completionState, false) ?? null);
  }

  update(update: ViewUpdate): void {
    const next = update.state.field(completionState, false) ?? null;
    if (next !== this.current) this.render(next);
  }

  private render(next: ActiveCompletion | null): void {
    if (!next) return;
    const prev = this.current;
    this.current = next;
    if (prev && prev.options === next.options) {
      this.item(prev.selected)?.setAttribute('aria-selected', 'false');
    } else {
      this.rebuild(next);
    }
    const selected = this.item(next.selected);
    selected?.setAttribute('aria-selected', 'true');
    if (selected) this.scrollToSelected(selected);
  }

  private item(index: number): HTMLElement | null {
    return (this.list.children[index] as HTMLElement | undefined) ?? null;
  }

  private rebuild(active: ActiveCompletion): void {
    const doc = this.list.ownerDocument;
    const frag = doc.createDocumentFragment();
    active.options.forEach((option, i) => {
      const li = doc.createElement('li');
      li.id = `${this.id}-${i}`;
      li.dataset.index = String(i);
      li.setAttribute('role', 'option');
      li.setAttribute('aria-selected', 'false');
      const type = option.type?.split(' ')[0] ?? '';
      const icon = doc.createElement('span');
      icon.className = 'cm-sql-completion-icon';
      icon.dataset.type = type;
      icon.textContent = TYPE_ICON[type] ?? '·';
      icon.title = TYPE_TITLE[type] ?? type;
      icon.setAttribute('aria-hidden', 'true');
      const label = doc.createElement('span');
      label.className = 'cm-sql-completion-label';
      label.textContent = option.displayLabel ?? option.label;
      li.append(icon, label);
      if (option.detail) {
        const detail = doc.createElement('span');
        detail.className = 'cm-sql-completion-detail';
        detail.textContent = option.detail;
        li.append(detail);
      }
      frag.append(li);
    });
    this.list.replaceChildren(frag);
    this.list.scrollTop = 0;
  }

  /** Keep the selected row visible; layout is read and written in CodeMirror's measure phase. */
  private scrollToSelected(item: HTMLElement): void {
    this.view.requestMeasure({
      key: this,
      read: () => ({ top: item.offsetTop, height: item.offsetHeight, scroll: this.list.scrollTop, view: this.list.clientHeight }),
      write: (m) => {
        if (m.top < m.scroll) this.list.scrollTop = m.top;
        else if (m.top + m.height > m.scroll + m.view) this.list.scrollTop = m.top + m.height - m.view;
      },
    });
  }
}

function createListView(view: EditorView): TooltipView {
  return new CompletionListView(view);
}

// ---- extension -----------------------------------------------------------------------------

/**
 * The completion popup: state, tooltip, keys, ARIA attributes and close-on-blur. Sources are
 * whatever registers `autocomplete` language data (`sql()` from `@codemirror/lang-sql` does).
 */
export function sqlCompletion(): Extension {
  return [
    completionState,
    listId,
    Prec.highest(
      keymap.of([
        { key: 'Ctrl-Space', run: startSqlCompletion },
        // macOS often keeps Ctrl-Space for switching input sources; CodeMirror's alternative.
        { mac: 'Alt-`', run: startSqlCompletion },
        { key: 'ArrowDown', run: moveBy(1, true) },
        { key: 'ArrowUp', run: moveBy(-1, true) },
        { key: 'PageDown', run: moveBy(PAGE, false) },
        { key: 'PageUp', run: moveBy(-PAGE, false) },
        { key: 'Enter', run: (view) => acceptSqlCompletion(view) },
        { key: 'Tab', run: (view) => acceptSqlCompletion(view) },
        { key: 'Escape', run: closeSqlCompletion },
      ]),
    ),
    EditorView.focusChangeEffect.of((state, focusing) => (!focusing && state.field(completionState, false) ? closeEffect.of(null) : null)),
    EditorView.contentAttributes.compute([completionState, listId], (state): Record<string, string> => {
      const active = state.field(completionState);
      const base = { 'aria-autocomplete': 'list', 'aria-haspopup': 'listbox' };
      if (!active) return base;
      const id = state.field(listId);
      return { ...base, 'aria-controls': id, 'aria-activedescendant': `${id}-${active.selected}` };
    }),
  ];
}
