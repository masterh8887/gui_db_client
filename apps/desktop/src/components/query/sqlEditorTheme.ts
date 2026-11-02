// SQL editor look (DESIGN.md §3.1, §3.3): an EditorView theme and a syntax highlighter that use
// only CSS variables from `app/globals.css`, so the editor follows the light / dark theme (and a
// theme switch) without being reconfigured. Tokens use the Xcode-like `--syn-*` palette only,
// never `--accent`, `--danger`, `--success` or `--warning`. Mono 13/20, tertiary gutter without a
// border, active line `--bg-hover`, selection `--syn-selection`.

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
  'function(variableName)': 'cm-sql-function',
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

/** DESIGN.md §3.2 `--font-mono` (repeated: `@theme inline` does not emit the variable). */
const MONO = 'ui-monospace, "SF Mono", "Cascadia Mono", "Cascadia Code", Consolas, "D2Coding", "Malgun Gothic", monospace';

/** The theme spec, exported so a test can assert which CSS variables it uses. */
export const SQL_EDITOR_THEME_SPEC = {
  '&': {
    height: '100%',
    fontSize: '13px',
    color: 'var(--fg)',
    backgroundColor: 'var(--bg-window)',
  },
  '&.cm-focused': { outline: 'none' },
  '.cm-scroller': { fontFamily: MONO, lineHeight: '20px', overflow: 'auto' },
  '.cm-content': { caretColor: 'var(--fg)', padding: '6px 0' },
  '.cm-line': { padding: '0 12px 0 6px' },
  // Native selection and caret (no drawSelection layer: nothing to measure per keystroke).
  '.cm-content ::selection': { backgroundColor: 'var(--syn-selection)' },
  '.cm-activeLine': { backgroundColor: 'var(--bg-hover)' },
  '.cm-gutters': {
    backgroundColor: 'var(--bg-window)',
    color: 'var(--fg-tertiary)',
    border: 'none',
  },
  '.cm-lineNumbers .cm-gutterElement': { padding: '0 6px 0 10px', minWidth: '32px', fontVariantNumeric: 'tabular-nums' },
  '.cm-activeLineGutter': { backgroundColor: 'transparent', color: 'var(--fg-secondary)' },
  '.cm-placeholder': { color: 'var(--fg-tertiary)' },
  '&.cm-focused .cm-matchingBracket': { backgroundColor: 'var(--syn-selection)', outline: '1px solid var(--separator-strong)' },
  '&.cm-focused .cm-nonmatchingBracket': { textDecoration: 'underline wavy var(--syn-string)' },

  // Tokens (weight 400 everywhere; comments italic).
  '.cm-sql-keyword': { color: 'var(--syn-keyword)' },
  '.cm-sql-type': { color: 'var(--syn-type)' },
  '.cm-sql-builtin': { color: 'var(--syn-type)' },
  '.cm-sql-function': { color: 'var(--syn-function)' },
  '.cm-sql-variable': { color: 'var(--syn-variable)' },
  '.cm-sql-quoted': { color: 'var(--fg)' },
  '.cm-sql-string': { color: 'var(--syn-string)' },
  '.cm-sql-number': { color: 'var(--syn-number)' },
  '.cm-sql-atom': { color: 'var(--syn-keyword)' },
  '.cm-sql-comment': { color: 'var(--syn-comment)', fontStyle: 'italic' },
  '.cm-sql-operator': { color: 'var(--fg)' },

  // Tooltips and the completion list (`sqlCompletion.ts`): an opaque popover like every menu.
  '.cm-tooltip': {
    backgroundColor: 'var(--bg-elevated)',
    color: 'var(--fg)',
    border: 'none',
    borderRadius: '8px',
    boxShadow: 'var(--elev-popover)',
  },
  '.cm-tooltip.cm-sql-completions': { padding: '4px', minWidth: '220px', maxWidth: '480px' },
  '.cm-sql-completions ul': {
    margin: '0',
    padding: '0',
    listStyle: 'none',
    maxHeight: '240px',
    overflowY: 'auto',
    fontFamily: MONO,
    fontSize: '12px',
  },
  '.cm-sql-completions li': {
    display: 'flex',
    alignItems: 'center',
    gap: '6px',
    height: '24px',
    padding: '0 8px 0 4px',
    borderRadius: '5px',
    cursor: 'default',
    whiteSpace: 'nowrap',
  },
  '.cm-sql-completions li[aria-selected="true"]': { backgroundColor: 'var(--bg-selected-focus)', color: 'var(--fg-on-accent)' },
  '.cm-sql-completions li[aria-selected="true"] .cm-sql-completion-detail': { color: 'var(--fg-on-accent-muted)' },
  '.cm-sql-completions li[aria-selected="true"] .cm-sql-completion-icon': { color: 'var(--fg-on-accent)' },
  '.cm-sql-completion-icon': {
    display: 'inline-flex',
    alignItems: 'center',
    justifyContent: 'center',
    width: '16px',
    height: '16px',
    flexShrink: '0',
    borderRadius: '4px',
    fontSize: '10px',
    fontWeight: '600',
    backgroundColor: 'var(--bg-control)',
  },
  '.cm-sql-completion-icon[data-type="keyword"]': { color: 'var(--syn-keyword)' },
  '.cm-sql-completion-icon[data-type="type"]': { color: 'var(--syn-type)' },
  '.cm-sql-completion-icon[data-type="namespace"]': { color: 'var(--syn-function)' },
  '.cm-sql-completion-icon[data-type="property"]': { color: 'var(--syn-variable)' },
  '.cm-sql-completion-label': { overflow: 'hidden', textOverflow: 'ellipsis' },
  '.cm-sql-completion-detail': {
    marginLeft: 'auto',
    paddingLeft: '12px',
    color: 'var(--fg-tertiary)',
    overflow: 'hidden',
    textOverflow: 'ellipsis',
  },
} as const;

/** Editor chrome, tokens and the completion popup, all from the app's CSS variables. */
export const sqlEditorTheme = EditorView.theme(SQL_EDITOR_THEME_SPEC);

/** Theme plus highlighter. */
export function sqlEditorLook(): Extension {
  return [sqlEditorTheme, syntaxHighlighting(sqlHighlighter)];
}
