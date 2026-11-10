'use client';

// Script palette shown in the right inspector (세부 정보) while a script tab is active. Three
// sections: the saved connections (click to open one by name), a language-aware snippet catalog,
// and an Excel/CSV export builder that previews the output structure (columns only — never data,
// per the app's "no data preview" rule) and inserts the matching code.

import type { ScriptRuntime } from 'ipc-types';
import { Database, Plus, Table2, X } from 'lucide-react';
import { useMemo, useState } from 'react';
import { cn } from '@/lib/utils';
import { Badge, Button, EnvTag, Input, SegmentedControl, Tooltip } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useScriptStore } from '@/stores/script';
import { connectionSnippet, exportCode, snippetGroups, type ExportSheet } from './snippets';
import { is } from './strings';

const DRIVER_LABEL: Record<string, string> = { postgres: 'PostgreSQL', mysql: 'MySQL', sqlite: 'SQLite', mssql: 'SQL Server', oracle: 'Oracle', odbc: 'ODBC' };
const driverLabel = (driver: string) => DRIVER_LABEL[driver] ?? driver;

function SectionTitle({ children }: { children: React.ReactNode }) {
  return <div className="px-3 pt-3 pb-1 text-xs font-semibold text-fg-secondary">{children}</div>;
}

// ----- connections ---------------------------------------------------------------------------

function ConnectionsSection({ tabId, connId, runtime }: { tabId: string; connId: string; runtime: ScriptRuntime }) {
  const connections = useConnectionsStore((s) => s.connections);
  const insert = (name: string, isTab: boolean) => useScriptStore.getState().insertSnippet(tabId, connectionSnippet(runtime, name, isTab));

  if (connections.length === 0) return null;
  return (
    <div>
      <SectionTitle>{is('connections')}</SectionTitle>
      <div className="px-1.5">
        {connections.map((c) => {
          const isTab = c.id === connId;
          return (
            <Tooltip key={c.id} content={runtime === 'node' ? `bulkdb.db('${c.name}')` : is('connInsertHint')} delay={500}>
              <button
                type="button"
                onClick={() => insert(c.name, isTab)}
                className="group flex w-full items-center gap-2 rounded-[5px] px-1.5 py-1 text-left hover:bg-hover active:bg-pressed"
              >
                <Database size={13} strokeWidth={1.75} className="shrink-0 text-fg-tertiary" />
                <span className="min-w-0 flex-1 truncate text-sm text-fg">{c.name}</span>
                {isTab && <span className="shrink-0 text-xs text-fg-tertiary">{is('thisTab')}</span>}
                {c.tag && <EnvTag tag={c.tag} variant="dot" />}
                <Badge tone="outline" className="shrink-0">
                  {driverLabel(c.driver)}
                </Badge>
                <Plus size={13} strokeWidth={2} className="shrink-0 text-fg-tertiary opacity-0 group-hover:opacity-100" />
              </button>
            </Tooltip>
          );
        })}
      </div>
    </div>
  );
}

// ----- snippets ------------------------------------------------------------------------------

function SnippetsSection({ tabId, runtime }: { tabId: string; runtime: ScriptRuntime }) {
  const groups = useMemo(() => snippetGroups(runtime), [runtime]);
  const insert = (code: string) => useScriptStore.getState().insertSnippet(tabId, code);

  return (
    <div>
      {groups.map((group) => (
        <div key={group.title}>
          <SectionTitle>{group.title}</SectionTitle>
          <div className="px-1.5">
            {group.items.map((item) => (
              <Tooltip key={item.id} content={item.hint} delay={400}>
                <button
                  type="button"
                  onClick={() => insert(item.code)}
                  className="group flex w-full items-center gap-2 rounded-[5px] px-1.5 py-1 text-left hover:bg-hover active:bg-pressed"
                >
                  <span className="min-w-0 flex-1 truncate text-sm text-fg">{item.label}</span>
                  <code className="min-w-0 max-w-[55%] shrink truncate font-mono text-xs text-fg-tertiary">{item.hint}</code>
                  <Plus size={13} strokeWidth={2} className="shrink-0 text-fg-tertiary opacity-0 group-hover:opacity-100" />
                </button>
              </Tooltip>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}

// ----- export builder + structure preview ----------------------------------------------------

interface SheetDraft {
  name: string;
  /** Raw comma / newline separated column text, parsed to columns on use. */
  cols: string;
}

function parseCols(raw: string): string[] {
  return raw
    .split(/[,\n]/)
    .map((c) => c.trim())
    .filter(Boolean);
}

function ExportSection({ tabId }: { tabId: string }) {
  const [format, setFormat] = useState<'xlsx' | 'csv'>('xlsx');
  const [filename, setFilename] = useState('out.xlsx');
  const [sheets, setSheets] = useState<SheetDraft[]>([{ name: 'Sheet1', cols: '' }]);

  const multi = format === 'xlsx';
  const previewSheets: ExportSheet[] = sheets.map((s) => ({ name: s.name.trim() || 'Sheet1', columns: parseCols(s.cols) }));
  const totalCols = previewSheets.reduce((n, s) => n + s.columns.length, 0);

  const setSheet = (i: number, patch: Partial<SheetDraft>) => setSheets((ss) => ss.map((s, k) => (k === i ? { ...s, ...patch } : s)));
  const addSheet = () => setSheets((ss) => [...ss, { name: `Sheet${ss.length + 1}`, cols: '' }]);
  const removeSheet = (i: number) => setSheets((ss) => (ss.length > 1 ? ss.filter((_, k) => k !== i) : ss));

  const changeFormat = (f: 'xlsx' | 'csv') => {
    setFormat(f);
    setFilename((name) => name.replace(/\.(xlsx|csv)$/i, '') + '.' + f);
    if (f === 'csv' && sheets.length > 1) setSheets((ss) => ss.slice(0, 1));
  };

  const insert = () => {
    const defined = previewSheets.filter((s) => s.columns.length || sheets.length > 1);
    useScriptStore.getState().insertSnippet(tabId, exportCode(defined.length ? defined : previewSheets, filename.trim() || 'out.' + format, format));
  };

  return (
    <div>
      <SectionTitle>{is('exportTitle')}</SectionTitle>
      <div className="flex flex-col gap-2 px-3 pb-3">
        <div className="flex items-center gap-2">
          <SegmentedControl
            aria-label={is('exportFormat')}
            size="sm"
            value={format}
            onChange={changeFormat}
            options={[
              { value: 'xlsx', label: 'XLSX' },
              { value: 'csv', label: 'CSV' },
            ]}
          />
          <Input aria-label={is('exportFile')} value={filename} onChange={(e) => setFilename(e.target.value)} dense className="flex-1 font-mono text-xs" />
        </div>

        {sheets.map((sheet, i) => (
          <div key={i} className="rounded-[6px] border border-separator bg-bg p-2">
            <div className="flex items-center gap-1.5">
              <Table2 size={12} strokeWidth={1.75} className="shrink-0 text-fg-tertiary" />
              {multi ? (
                <Input aria-label={is('sheetName')} value={sheet.name} onChange={(e) => setSheet(i, { name: e.target.value })} dense className="h-6 flex-1 text-xs" />
              ) : (
                <span className="flex-1 text-xs text-fg-secondary">{is('csvOneSheet')}</span>
              )}
              {multi && sheets.length > 1 && (
                <button type="button" aria-label={is('removeSheet')} onClick={() => removeSheet(i)} className="rounded p-0.5 text-fg-tertiary hover:bg-hover hover:text-fg">
                  <X size={12} />
                </button>
              )}
            </div>
            <Input
              aria-label={is('sheetColumns')}
              value={sheet.cols}
              onChange={(e) => setSheet(i, { cols: e.target.value })}
              placeholder={is('columnsPlaceholder')}
              dense
              className="mt-1.5 h-6 w-full font-mono text-xs"
            />
            <ColumnPreview columns={parseCols(sheet.cols)} />
          </div>
        ))}

        {multi && (
          <Button variant="ghost" size="sm" onClick={addSheet} className="self-start">
            <Plus size={13} strokeWidth={2} className="mr-1" />
            {is('addSheet')}
          </Button>
        )}

        <div className="text-xs text-fg-tertiary">{is('exportSummary', String(previewSheets.length), String(totalCols))}</div>
        <Button variant="secondary" size="sm" onClick={insert}>
          {is('insertExport')}
        </Button>
      </div>
    </div>
  );
}

/** Header-only preview: the planned columns, never any data rows. */
function ColumnPreview({ columns }: { columns: string[] }) {
  if (columns.length === 0) return null;
  return (
    <div className="mt-1.5 overflow-hidden rounded-[4px] border border-separator">
      <div className="flex bg-subtle">
        {columns.map((c, i) => (
          <div key={i} className={cn('min-w-0 flex-1 truncate border-separator px-1.5 py-0.5 font-mono text-xs font-medium text-fg', i > 0 && 'border-l')}>
            {c}
          </div>
        ))}
      </div>
      <div className="flex border-t border-separator">
        {columns.map((_, i) => (
          <div key={i} className={cn('min-w-0 flex-1 border-separator px-1.5 py-0.5 text-xs text-fg-placeholder', i > 0 && 'border-l')}>
            …
          </div>
        ))}
      </div>
    </div>
  );
}

// ----- root ----------------------------------------------------------------------------------

export function ScriptInspector({ tabId, connId }: { tabId: string; connId: string }) {
  const runtime = useScriptStore((s) => s.docs[tabId]?.runtime ?? 'node');
  const node = runtime === 'node';
  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-y-auto pb-3">
      <ConnectionsSection tabId={tabId} connId={connId} runtime={runtime} />
      <SnippetsSection tabId={tabId} runtime={runtime} />
      {node && <ExportSection tabId={tabId} />}
    </div>
  );
}
