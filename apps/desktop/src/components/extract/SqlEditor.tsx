'use client';

// SQL editor (CodeMirror 6). Controlled: `value` is the source of truth and `onChange` reports
// user edits. An external `value` change is applied as a minimal diff only when it differs from
// the document, so typing never makes the cursor jump.
//
// Highlighting uses the dialect of the target connection's driver; completion offers keywords and
// the schema / table / column names the catalog store already holds (the editor never loads
// metadata itself). Mod-Enter runs and Mod-E explains: through `onRun` / `onExplain` when given,
// otherwise the keys reach the app-wide shortcuts (AppShell), which do the same. The editor shows
// SQL only; there are no query results anywhere in this app.

import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { sql } from '@codemirror/lang-sql';
import { bracketMatching, indentOnInput, indentUnit } from '@codemirror/language';
import { Annotation, Compartment, EditorState, Prec, type Extension } from '@codemirror/state';
import {
  EditorView,
  highlightActiveLine,
  highlightActiveLineGutter,
  keymap,
  lineNumbers,
  placeholder as placeholderText,
  type Command,
} from '@codemirror/view';
import { useEffect, useMemo, useRef, useSyncExternalStore } from 'react';
import { cn } from '@/lib/utils';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
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
  /**
   * Connection whose driver dialect and cached catalog drive highlighting and completion.
   * Default: the extract target, else the connection selected in the tree.
   */
  connId?: string | null;
}

/** Marks the transaction that applies an external `value`, so it is not echoed to `onChange`. */
const externalValue = Annotation.define<boolean>();

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
  connId,
  'aria-label': ariaLabel,
}: SqlEditorProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const compartmentsRef = useRef<Compartments | null>(null);

  // Target connection -> dialect and completion schema (from the catalog cache only).
  const extractConnId = useExtractStore((s) => s.connId);
  const selectedId = useConnectionsStore((s) => s.selectedId);
  const targetId = connId !== undefined ? connId : (extractConnId ?? selectedId);
  const conn = useConnectionsStore((s) => s.connections.find((c) => c.id === targetId));
  const databases = useCatalogStore((s) => s.databases);
  const schemas = useCatalogStore((s) => s.schemas);
  const tables = useCatalogStore((s) => s.tables);
  const details = useCatalogStore((s) => s.details);
  const dark = useDarkMode();

  const driver = conn?.driver;
  const database = conn?.database;
  const user = conn?.user;
  const dialectName = dialectNameFor(driver);

  const languageExt = useMemo<Extension>(() => {
    const dialect = dialectFor(dialectName);
    const schema = buildSqlSchema({ databases, schemas, tables, details }, { connId: targetId, driver, database, user }, dialect);
    return [
      sql({ dialect, schema: schema.namespace, defaultSchema: schema.defaultSchema, upperCaseKeywords: true }),
      dialect.language.data.of({ autocomplete: fromClauseColumnsSource(schema, dialect) }),
    ];
  }, [dialectName, databases, schemas, tables, details, targetId, driver, database, user]);

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
  const latest = useRef({ value, onChange, onRun, onExplain, languageExt, readOnlyExt, labelsExt, darkExt });
  useEffect(() => {
    latest.current = { value, onChange, onRun, onExplain, languageExt, readOnlyExt, labelsExt, darkExt };
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
              { key: 'Mod-e', run: shortcut(() => latest.current.onExplain) },
            ]),
          ),
          // Mod-Enter is "insert blank line" in the default keymap; here it always means run.
          keymap.of([...defaultKeymap.filter((b) => b.key !== 'Mod-Enter'), ...historyKeymap, indentWithTab]),
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

  return (
    <div
      ref={hostRef}
      data-dialect={dialectName}
      data-readonly={readOnly || undefined}
      className={cn('h-full min-h-0 w-full overflow-hidden bg-background text-[13px]', className)}
    />
  );
}
