'use client';

// SQL editor + session/output options + Run / EXPLAIN. The actual flow lives in the extract
// store; this component only renders state and forwards clicks.

import { FileText, FolderOpen, Play, Search, ShieldCheck } from 'lucide-react';
import { useEffect } from 'react';
import { pickSqlFile } from '@/lib/dialogs';
import { formatBytes, formatDateTime } from '@/lib/format';
import { t } from '@/lib/i18n';
import { shortcutLabel } from '@/lib/platform';
import { Badge, Button, Input, Kbd, Notice } from '@/components/ui';
import { selectDriver, useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { OutputOptionsForm } from './OutputOptionsForm';
import { PlanSummaryView } from './PlanSummaryView';
import { SessionOptionsForm } from './SessionOptionsForm';
import { SqlEditor } from './SqlEditor';

export function ExtractTab() {
  const sqlMode = useExtractStore((s) => s.sqlMode);
  const sql = useExtractStore((s) => s.sql);
  const sqlFile = useExtractStore((s) => s.sqlFile);
  const sqlFileInfo = useExtractStore((s) => s.sqlFileInfo);
  const name = useExtractStore((s) => s.name);
  const busy = useExtractStore((s) => s.busy);
  const error = useExtractStore((s) => s.error);
  const lastCheck = useExtractStore((s) => s.lastCheck);
  const lastPlan = useExtractStore((s) => s.lastPlan);
  const extractConnId = useExtractStore((s) => s.connId);
  const setSqlMode = useExtractStore((s) => s.setSqlMode);
  const setSql = useExtractStore((s) => s.setSql);
  const setSqlFile = useExtractStore((s) => s.setSqlFile);
  const setName = useExtractStore((s) => s.setName);
  const setConnId = useExtractStore((s) => s.setConnId);
  const clearError = useExtractStore((s) => s.clearError);
  const run = useExtractStore((s) => s.run);
  const explain = useExtractStore((s) => s.explain);
  const check = useExtractStore((s) => s.check);
  const loadSqlFile = useExtractStore((s) => s.loadSqlFile);

  const selectedId = useConnectionsStore((s) => s.selectedId);
  const conn = useConnectionsStore((s) => s.connections.find((c) => c.id === (extractConnId ?? s.selectedId)));
  const driver = useConnectionsStore((s) => selectDriver(s, conn?.driver));

  // Follow the tree selection; `setConnId` applies that connection's session/output defaults.
  useEffect(() => {
    if (selectedId && extractConnId !== selectedId) setConnId(selectedId);
  }, [selectedId, extractConnId, setConnId]);

  const browse = async () => {
    const path = await pickSqlFile();
    if (path) await loadSqlFile(path);
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex min-h-9 shrink-0 flex-wrap items-center gap-1 border-b border-border px-2 py-1">
        <div className="flex shrink-0 items-center overflow-hidden rounded border border-border">
          <button
            type="button"
            className={`h-6 whitespace-nowrap px-2 text-xs ${sqlMode === 'text' ? 'bg-accent text-accent-foreground' : 'text-muted-foreground hover:bg-muted'}`}
            onClick={() => setSqlMode('text')}
            aria-pressed={sqlMode === 'text'}
          >
            {t('extract.sqlText')}
          </button>
          <button
            type="button"
            className={`h-6 whitespace-nowrap px-2 text-xs ${sqlMode === 'file' ? 'bg-accent text-accent-foreground' : 'text-muted-foreground hover:bg-muted'}`}
            onClick={() => setSqlMode('file')}
            aria-pressed={sqlMode === 'file'}
          >
            {t('extract.sqlFile')}
          </button>
        </div>
        <div className="w-40 shrink-0">
          <Input dense aria-label={t('extract.name')} value={name} onChange={(e) => setName(e.target.value)} placeholder={t('extract.namePlaceholder')} />
        </div>
        {conn && (
          <Badge tone="outline" title={conn.driver}>
            {conn.name}
          </Badge>
        )}
        <div className="ml-auto flex shrink-0 items-center gap-1">
          <Button size="sm" variant="ghost" icon={<ShieldCheck size={13} />} onClick={() => void check()} loading={busy === 'check'} disabled={!!busy && busy !== 'check'}>
            {t('extract.check')}
          </Button>
          <Button size="sm" variant="outline" icon={<Search size={13} />} onClick={() => void explain()} loading={busy === 'explain'} disabled={!!busy && busy !== 'explain'}>
            {t('extract.explain')} <Kbd>{shortcutLabel('E')}</Kbd>
          </Button>
          <Button size="sm" variant="primary" icon={<Play size={13} />} onClick={() => void run()} loading={busy === 'preflight' || busy === 'starting'} disabled={!!busy && busy !== 'preflight' && busy !== 'starting'}>
            {t('extract.run')} <Kbd inverse>{shortcutLabel('Enter')}</Kbd>
          </Button>
        </div>
      </div>

      <div className="flex min-h-0 flex-1 flex-col overflow-auto">
        <div className="flex h-[42%] min-h-[150px] shrink-0 flex-col border-b border-border">
          {sqlMode === 'text' ? (
            <SqlEditor value={sql} onChange={setSql} placeholder={t('extract.sqlPlaceholder')} aria-label={t('extract.sql')} />
          ) : (
            <div className="flex h-full flex-col gap-2 p-2">
              <div className="flex items-center gap-1">
                <FileText size={14} className="text-muted-foreground" />
                <Input aria-label={t('extract.sqlFile')} value={sqlFile} onChange={(e) => setSqlFile(e.target.value)} placeholder={t('extract.filePlaceholder')} />
                <Button size="md" variant="outline" icon={<FolderOpen size={13} />} onClick={() => void browse()}>
                  {t('extract.browse')}
                </Button>
                <Button size="md" variant="secondary" onClick={() => void loadSqlFile()} loading={busy === 'file'} disabled={!sqlFile.trim()}>
                  {t('extract.loadFile')}
                </Button>
              </div>
              {sqlFileInfo ? (
                <>
                  <div className="text-xs text-muted-foreground">
                    {formatBytes(sqlFileInfo.sizeBytes)} · {formatDateTime(sqlFileInfo.modifiedAt, true)}
                  </div>
                  <SqlEditor value={sqlFileInfo.content} onChange={() => undefined} readOnly className="min-h-0 flex-1 rounded border border-border" />
                </>
              ) : (
                <div className="text-xs text-muted-foreground">{t('extract.filePlaceholder')}</div>
              )}
            </div>
          )}
        </div>

        <div className="flex flex-col gap-2 p-2">
          {error && (
            <Notice tone="danger" onClose={clearError}>
              {error}
            </Notice>
          )}
          {lastCheck && (
            lastCheck.ok ? (
              <Notice tone="success">
                {t('extract.sqlOk')} · {lastCheck.kind.toUpperCase()}
              </Notice>
            ) : (
              <Notice tone="danger" title={t('extract.sqlIssues')}>
                <ul className="list-disc pl-4">
                  {lastCheck.issues.map((i, n) => (
                    <li key={`${i.code}-${n}`}>
                      <Badge tone="outline">{i.code}</Badge> {i.message}
                    </li>
                  ))}
                </ul>
              </Notice>
            )
          )}
          {/* Above the option forms so an EXPLAIN result is visible without scrolling. */}
          {lastPlan && <PlanSummaryView plan={lastPlan} />}
          <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
            <SessionOptionsForm driver={driver} />
            <OutputOptionsForm />
          </div>
        </div>
      </div>
    </div>
  );
}
