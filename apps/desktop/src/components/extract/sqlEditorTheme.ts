// SQL editor look: an EditorView theme and a syntax highlighter that use only the CSS variables
// from `app/globals.css`, so the editor follows the light / dark theme (and a theme switch)
// without being reconfigured. 13px monospace, 20px lines, desktop density.

import { syntaxHighlighting } from '@codemirror/language';
import type { Extension } from '@codemirror/state';
import { EditorView } from '@codemirror/view';

type Highlighter = Parameters<typeof syntaxHighlighting>[0];

/**
 * Highlighting tag (by its printed name, e.g. `keyword`, `special(string)`) -> CSS class. Tags
 * are matched through their parent chain (`lineComment` -> `comment`), most specific first.
 * The tag objects themselves live in `@lezer/highlight`, which this app does not depend on
 * directly, hence the match by name.
 */
const TOKEN_CLASS: Record<string, string> = {
  keyword: 'cm-sql-keyword',
  typeName: 'cm-sql-type',
  'standard(name)': 'cm-sql-builtin',
  'special(name)': 'cm-sql-variable',
  'special(string)': 'cm-sql-quoted',
  string: 'cm-sql-string',
  number: 'cm-sql-number',
  bool: 'cm-sql-atom',
  null: 'cm-sql-atom',
  comment: 'cm-sql-comment',
  operator: 'cm-sql-operator',
};

/** Maps lang-sql's highlighting tags to the `cm-sql-*` classes styled by `sqlEditorTheme`. */
export const sqlHighlighter: Highlighter = {
  style(tags) {
    for (const tag of tags) {
      for (const t of tag.set) {
        const cls = TOKEN_CLASS[String(t)];
        if (cls) return cls;
      }
    }
    return null;
  },
};

const MONO = 'ui-monospace, SFMono-Regular, Menlo, Consolas, "D2Coding", "Liberation Mono", monospace';
const mix = (color: string, pct: number) => `color-mix(in srgb, ${color} ${pct}%, transparent)`;

/** Editor chrome, tokens and the completion popup, all from the app's CSS variables. */
export const sqlEditorTheme = EditorView.theme({
  '&': {
    height: '100%',
    fontSize: '13px',
    color: 'var(--foreground)',
    backgroundColor: 'var(--background)',
  },
  '&.cm-focused': { outline: 'none' },
  '.cm-scroller': { fontFamily: MONO, lineHeight: '20px', overflow: 'auto' },
  '.cm-content': { caretColor: 'var(--foreground)', padding: '4px 0' },
  '.cm-line': { padding: '0 8px' },
  // Native selection and caret (no drawSelection layer: nothing to measure per keystroke).
  '.cm-content ::selection': { backgroundColor: mix('var(--accent)', 28) },
  '.cm-activeLine': { backgroundColor: mix('var(--accent)', 6) },
  '.cm-gutters': {
    backgroundColor: 'var(--muted)',
    color: mix('var(--muted-foreground)', 75),
    borderRight: '1px solid var(--border)',
  },
  '.cm-lineNumbers .cm-gutterElement': { padding: '0 8px 0 6px', minWidth: '28px', fontVariantNumeric: 'tabular-nums' },
  '.cm-activeLineGutter': { backgroundColor: mix('var(--accent)', 12), color: 'var(--foreground)' },
  '.cm-placeholder': { color: mix('var(--muted-foreground)', 70) },
  '&.cm-focused .cm-matchingBracket': { backgroundColor: mix('var(--accent)', 18), outline: `1px solid ${mix('var(--accent)', 45)}` },
  '&.cm-focused .cm-nonmatchingBracket': { backgroundColor: mix('var(--danger)', 20) },

  // Tokens.
  '.cm-sql-keyword': { color: 'var(--accent)', fontWeight: '600' },
  '.cm-sql-type': { color: 'var(--success)' },
  '.cm-sql-builtin': { color: 'var(--warning)' },
  '.cm-sql-variable': { color: 'var(--warning)', fontStyle: 'italic' },
  '.cm-sql-quoted': { color: 'var(--foreground)', textDecoration: `underline dotted ${mix('var(--muted-foreground)', 60)}` },
  '.cm-sql-string': { color: 'var(--danger)' },
  '.cm-sql-number': { color: 'var(--success)' },
  '.cm-sql-atom': { color: 'var(--accent)', fontStyle: 'italic' },
  '.cm-sql-comment': { color: 'var(--muted-foreground)', fontStyle: 'italic' },
  '.cm-sql-operator': { color: 'var(--muted-foreground)' },

  // Tooltips and the completion list (`sqlCompletion.ts`).
  '.cm-tooltip': {
    backgroundColor: 'var(--background)',
    color: 'var(--foreground)',
    border: '1px solid var(--border)',
    borderRadius: '4px',
    boxShadow: '0 4px 14px rgba(0, 0, 0, 0.18)',
  },
  '.cm-tooltip.cm-sql-completions': { padding: '2px 0', minWidth: '220px', maxWidth: '480px' },
  '.cm-sql-completions ul': {
    margin: '0',
    padding: '0',
    listStyle: 'none',
    maxHeight: '224px',
    overflowY: 'auto',
    fontFamily: MONO,
    fontSize: '12px',
  },
  '.cm-sql-completions li': {
    display: 'flex',
    alignItems: 'center',
    gap: '6px',
    height: '22px',
    padding: '0 8px 0 4px',
    cursor: 'pointer',
    whiteSpace: 'nowrap',
  },
  '.cm-sql-completions li[aria-selected="true"]': { backgroundColor: 'var(--accent)', color: 'var(--accent-foreground)' },
  '.cm-sql-completions li[aria-selected="true"] .cm-sql-completion-detail': { color: 'inherit', opacity: '0.85' },
  '.cm-sql-completion-icon': {
    display: 'inline-flex',
    alignItems: 'center',
    justifyContent: 'center',
    width: '16px',
    height: '16px',
    flexShrink: '0',
    borderRadius: '3px',
    fontSize: '10px',
    fontWeight: '700',
    backgroundColor: mix('var(--muted-foreground)', 18),
  },
  '.cm-sql-completion-icon[data-type="keyword"]': { color: 'var(--accent)' },
  '.cm-sql-completion-icon[data-type="type"]': { color: 'var(--success)' },
  '.cm-sql-completion-icon[data-type="namespace"]': { color: 'var(--warning)' },
  '.cm-sql-completion-icon[data-type="property"]': { color: 'var(--foreground)' },
  '.cm-sql-completion-label': { overflow: 'hidden', textOverflow: 'ellipsis' },
  '.cm-sql-completion-detail': {
    marginLeft: 'auto',
    paddingLeft: '12px',
    color: 'var(--muted-foreground)',
    overflow: 'hidden',
    textOverflow: 'ellipsis',
  },
});

/** Theme plus highlighter. */
export function sqlEditorLook(): Extension {
  return [sqlEditorTheme, syntaxHighlighting(sqlHighlighter)];
}
