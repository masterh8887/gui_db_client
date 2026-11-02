'use client';

// SQL editor (CodeMirror 6). Controlled: `value` is the source of truth and `onChange` reports
// user edits. An external `value` change is applied as a minimal diff only when it differs from
// the document, so typing never makes the cursor jump.
//
// Highlighting uses the dialect of the doc connection's driver (`connId`, explicit); completion
// offers keywords and the schema / table / column names the catalog store already holds (the
// editor never loads metadata itself). Mod-Enter runs, Mod-E explains and Mod-Shift-Enter checks:
// through `onRun` / `onExplain` / `onCheck` when given, otherwise the keys reach the app-wide
// shortcuts, which do the same for the active tab. Everything else is CodeMirror's default keymap
// (Ctrl-Space completion, Mod-/ comment, undo). The editor shows SQL only; there are no query
// results anywhere in this app (DESIGN.md §2.9).

import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { sql } from '@codemirror/lang-sql';
import { bracketMatching, indentOnInput, indentUnit } from '@codemirror/language';
import { Annotation, Compartment, EditorSelection, EditorState, Prec, type Extension } from '@codemirror/state';
import {
  EditorView,
  drawSelection,
  highlightActiveLine,
  highlightActiveLineGutter,
  keymap,
  lineNumbers,
  placeholder as placeholderText,
  type Command,
} from '@codemirror/view';
import { useEffect, useMemo, useRef, useSyncExternalStore } from 'react';
import { useExecDatabase } from '@/lib/table-target';
import { cn } from '@/lib/utils';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useUiStore } from '@/stores/ui';
import { sqlCompletion } from './sqlCompletion';
import { sqlEditorLook } from './sqlEditorTheme';
import { buildSqlSchema, dialectFor, dialectNameFor, fromClauseColumnsSource } from './sqlSchema';

export interface SqlEditorProps {
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  readOnly?: boolean;
  className?: string;
  'aria-label'?: string;
  /**
   * Mod-Enter (Ctrl+Enter / Cmd+Enter). When omitted the key is left to the app-wide shortcut,
   * which runs the extraction.
   */
  onRun?: () => void;
  /** Mod-E. When omitted the key is left to the app-wide shortcut, which runs EXPLAIN. */
  onExplain?: () => void;
  /** Mod-Shift-Enter. When omitted the key is left to the app-wide shortcut, which checks the SQL. */
  onCheck?: () => void;
  /** Connection whose driver dialect and cached catalog drive highlighting and completion. */
  connId: string | null;
  /**
   * Whether the editor is on screen. Query tabs stay mounted while hidden (DESIGN.md §4); turning
   * visible asks CodeMirror to measure again (whatever it read while hidden was zero) and takes
   * focus if the switch left it nowhere (on <body> or on an element now hidden).
   */
  visible?: boolean;
  /** Focus the editor once, right after it is created. */
  autoFocus?: boolean;
}

/** Marks the transaction that applies an external `value`, so it is not echoed to `onChange`. */
const externalValue = Annotation.define<boolean>();

/**
 * Ctrl+D / Cmd+D: add the next occurrence of the selected text to the selection (VS Code /
 * Sublime style). If the selection is empty, select the word under the cursor first.
 * Wraps around the document. Already-selected ranges are skipped.
 */
const selectNextOccurrence: Command = (view) => {
  const { state } = view;
  const main = state.selection.main;

  // Empty selection → select current word first
  if (main.empty) {
    const word = state.wordAt(main.from);
    if (!word || word.from === word.to) return false;
    view.dispatch({ selection: EditorSelection.range(word.from, word.to), scrollIntoView: true });
    return true;
  }

  const text = state.doc.sliceString(main.from, main.to);
  if (!text) return false;
  const docStr = state.doc.toString();
  const len = text.length;

  // Search from after the rightmost selected end, then wrap around
  const searchFrom = Math.max(...state.selection.ranges.map((r) => r.to));
  const isAlreadySelected = (p: number) => state.selection.ranges.some((r) => r.from === p && r.to === p + len);

  let pos = docStr.indexOf(text, searchFrom);
  if (pos !== -1 && isAlreadySelected(pos)) pos = docStr.indexOf(text, pos + 1);
  // Wrap around
  if (pos === -1) {
    pos = docStr.indexOf(text, 0);
    while (pos !== -1 && isAlreadySelected(pos)) pos = docStr.indexOf(text, pos + 1);
  }
  if (pos === -1) return false;

  view.dispatch({
    selection: EditorSelection.create([...state.selection.ranges, EditorSelection.range(pos, pos + len)], state.selection.ranges.length),
    scrollIntoView: true,
  });
  return true;
};

/** Smallest single replacement turning `a` into `b` (common prefix and suffix kept). */
export function minimalChange(a: string, b: string): { from: number; to: number; insert: string } {
  const max = Math.min(a.length, b.length);
  let start = 0;
  while (start < max && a.charCodeAt(start) === b.charCodeAt(start)) start++;
  let endA = a.length;
  let endB = b.length;
  while (endA > start && endB > start && a.charCodeAt(endA - 1) === b.charCodeAt(endB - 1)) {
    endA--;
    endB--;
  }
  return { from: start, to: endA, insert: b.slice(start, endB) };
}

// ---- effective dark mode (CodeMirror's `darkTheme` flag for its base styles) ---------------

const DARK_QUERY = '(prefers-color-scheme: dark)';

function subscribeColorScheme(onChange: () => void): () => void {
  if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return () => undefined;
  const mql = window.matchMedia(DARK_QUERY);
  mql.addEventListener('change', onChange);
  return () => mql.removeEventListener('change', onChange);
}

function systemPrefersDark(): boolean {
  return typeof window !== 'undefined' && typeof window.matchMedia === 'function' && window.matchMedia(DARK_QUERY).matches;
}

function useDarkMode(): boolean {
  const theme = useUiStore((s) => s.theme);
  const systemDark = useSyncExternalStore(subscribeColorScheme, systemPrefersDark, () => false);
  return theme === 'dark' || (theme === 'system' && systemDark);
}

// ---- focus ---------------------------------------------------------------------------------

/**
 * Whether keyboard focus is nowhere useful: on <body>, or on an element that is no longer
 * rendered (inside a `hidden` / display:none ancestor, like the editor of the tab just left;
 * browsers do not always move focus off such an element right away).
 */
function focusIsLost(): boolean {
  if (typeof document === 'undefined') return false;
  const active = document.activeElement;
  if (!active || active === document.body || active === document.documentElement || !active.isConnected) return true;
  const check = (active as Partial<Pick<Element, 'checkVisibility'>>).checkVisibility;
  if (typeof check === 'function') return !check.call(active);
  return active.closest('[hidden]') !== null;
}

// ---- component -----------------------------------------------------------------------------

interface Compartments {
  language: Compartment;
  readOnly: Compartment;
  labels: Compartment;
  dark: Compartment;
}

export function SqlEditor({
  value,
  onChange,
  placeholder,
  readOnly = false,
  className,
  onRun,
  onExplain,
  onCheck,
  connId,
  visible = true,
  autoFocus = false,
  'aria-label': ariaLabel,
}: SqlEditorProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const compartmentsRef = useRef<Compartments | null>(null);

  // Doc connection -> dialect and completion schema (from the catalog cache only).
  const conn = useConnectionsStore((s) => s.connections.find((c) => c.id === connId));
  // Where queries run (conn.database, else the probed / current database): its schemas complete.
  const execDb = useExecDatabase(connId);
  const databases = useCatalogStore((s) => s.databases);
  const schemas = useCatalogStore((s) => s.schemas);
  const tables = useCatalogStore((s) => s.tables);
  const details = useCatalogStore((s) => s.details);
  const dark = useDarkMode();

  const driver = conn?.driver;
  const database = execDb ?? conn?.database;
  const user = conn?.user;
  const dialectName = dialectNameFor(driver);

  const languageExt = useMemo<Extension>(() => {
    const dialect = dialectFor(dialectName);
    const schema = buildSqlSchema({ databases, schemas, tables, details }, { connId, driver, database, user }, dialect);
    return [
      sql({ dialect, schema: schema.namespace, defaultSchema: schema.defaultSchema, upperCaseKeywords: true }),
      dialect.language.data.of({ autocomplete: fromClauseColumnsSource(schema, dialect) }),
    ];
  }, [dialectName, databases, schemas, tables, details, connId, driver, database, user]);

  const readOnlyExt = useMemo<Extension>(
    () => [EditorState.readOnly.of(readOnly), EditorView.contentAttributes.of(readOnly ? { 'aria-readonly': 'true' } : {})],
    [readOnly],
  );
  const labelsExt = useMemo<Extension>(
    () => [EditorView.contentAttributes.of({ 'aria-label': ariaLabel ?? 'SQL' }), placeholder ? placeholderText(placeholder) : []],
    [ariaLabel, placeholder],
  );
  const darkExt = useMemo<Extension>(() => EditorView.darkTheme.of(dark), [dark]);

  // Latest props for the editor's callbacks and for the first mount; updated before any effect
  // below reads them (effects run in declaration order).
  const latest = useRef({ value, onChange, onRun, onExplain, onCheck, autoFocus, languageExt, readOnlyExt, labelsExt, darkExt });
  useEffect(() => {
    latest.current = { value, onChange, onRun, onExplain, onCheck, autoFocus, languageExt, readOnlyExt, labelsExt, darkExt };
  });

  // Create the view once; props flow in through the compartments below.
  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const init = latest.current;
    const compartments: Compartments = {
      language: new Compartment(),
      readOnly: new Compartment(),
      labels: new Compartment(),
      dark: new Compartment(),
    };
    const shortcut =
      (pick: () => (() => void) | undefined): Command =>
      () => {
        const handler = pick();
        if (!handler) return false; // let the key reach the app-wide shortcut
        handler();
        return true; // handled: CodeMirror prevents the browser default
      };
    const view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: init.value,
        extensions: [
          lineNumbers(),
          highlightActiveLineGutter(),
          highlightActiveLine(),
          drawSelection(),
          EditorState.allowMultipleSelections.of(true),
          history(),
          indentOnInput(),
          bracketMatching(),
          indentUnit.of('  '),
          EditorState.tabSize.of(2),
          sqlEditorLook(),
          sqlCompletion(),
          Prec.highest(
            keymap.of([
              { key: 'Mod-Enter', run: shortcut(() => latest.current.onRun) },
              { key: 'Mod-Shift-Enter', run: shortcut(() => latest.current.onCheck) },
              { key: 'Mod-e', run: shortcut(() => latest.current.onExplain) },
              { key: 'Mod-d', run: selectNextOccurrence },
            ]),
          ),
          // Mod-Enter is "insert blank line" in the default keymap; here it always means run.
          keymap.of([...defaultKeymap.filter((b) => b.key !== 'Mod-Enter' && b.key !== 'Mod-Shift-Enter'), ...historyKeymap, indentWithTab]),
          EditorView.updateListener.of((update) => {
            if (update.transactions.some((tr) => tr.docChanged && !tr.annotation(externalValue))) {
              latest.current.onChange(update.state.doc.toString());
            }
          }),
          compartments.language.of(init.languageExt),
          compartments.readOnly.of(init.readOnlyExt),
          compartments.labels.of(init.labelsExt),
          compartments.dark.of(init.darkExt),
        ],
      }),
    });
    viewRef.current = view;
    compartmentsRef.current = compartments;
    if (init.autoFocus) view.focus();
    return () => {
      view.destroy();
      viewRef.current = null;
      compartmentsRef.current = null;
    };
  }, []);

  // Controlled value: only an actual difference is dispatched, as a minimal replacement.
  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const current = view.state.doc.toString();
    if (current === value) return;
    view.dispatch({ changes: minimalChange(current, value), annotations: externalValue.of(true) });
  }, [value]);

  useEffect(() => {
    const view = viewRef.current;
    const c = compartmentsRef.current;
    if (!view || !c) return;
    const effects = [
      c.language.get(view.state) !== languageExt ? c.language.reconfigure(languageExt) : null,
      c.readOnly.get(view.state) !== readOnlyExt ? c.readOnly.reconfigure(readOnlyExt) : null,
      c.labels.get(view.state) !== labelsExt ? c.labels.reconfigure(labelsExt) : null,
      c.dark.get(view.state) !== darkExt ? c.dark.reconfigure(darkExt) : null,
    ].filter((e) => e !== null);
    if (effects.length > 0) view.dispatch({ effects });
  }, [languageExt, readOnlyExt, labelsExt, darkExt]);

  // Shown again after being `hidden`: sizes measured meanwhile were zero. A keyboard tab switch
  // (Ctrl+Tab, Mod+1…9, Mod+W) hides or unmounts the editor that had focus, leaving focus nowhere;
  // the editor coming on screen takes it, so typing goes on. Focus that is still somewhere visible
  // (the tab strip during arrow-key navigation, the sidebar, a mode toggle) is left alone.
  const wasVisible = useRef(visible);
  useEffect(() => {
    const shown = visible && !wasVisible.current;
    wasVisible.current = visible;
    if (!visible) return;
    const view = viewRef.current;
    view?.requestMeasure();
    if (shown && view && focusIsLost()) view.focus();
  }, [visible]);

  return (
    <div
      ref={hostRef}
      data-dialect={dialectName}
      data-readonly={readOnly || undefined}
      className={cn('h-full min-h-0 w-full overflow-hidden bg-window text-base', className)}
    />
  );
}
