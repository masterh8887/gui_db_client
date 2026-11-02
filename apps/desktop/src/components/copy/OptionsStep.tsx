'use client';

// Step 3 옵션 (docs/db-copy.md 7). Visible: 이미 있는 테이블 (건너뛰기 is the default; the two
// destructive choices in danger text), 내용, 검증, and a facts line of the MySQL defaults. The rest
// sits under a collapsed 고급 disclosure. Every choice is a native radio list (DESIGN.md "Copy
// wizard look": the steps bar is the wizard's only SegmentedControl). Every change goes through
// `setDraft`, which clears the plan.

import type { CopyEffective, CopyOptions, MysqlCopyOptions, NameMapping, PgCopyOptions } from 'ipc-types';
import { Plus, X } from 'lucide-react';
import { useId, useState, type ReactNode } from 'react';
import { Button, IconButton, Input, NumberInput, Select, Switch } from '@/components/ui';
import { cn } from '@/lib/utils';
import { useConnectionsStore } from '@/stores/connections';
import { useCopyStore } from '@/stores/copy';
import { familyOf, POLICIES } from './model';
import { Caption, Disclosure, RadioList } from './parts';
import { cs, k } from './strings';

const AUTO = '\u0001auto';
const CHARSETS = ['utf8mb4', 'utf8', 'euckr', 'latin1'];
const COLLATIONS = ['utf8mb4_0900_ai_ci', 'utf8mb4_unicode_520_ci', 'utf8mb4_unicode_ci', 'utf8mb4_general_ci', 'utf8mb4_bin'];

/**
 * The MySQL facts line: "utf8mb4 · utf8mb4_0900_ai_ci · InnoDB · 0 날짜: NULL로". Before a plan
 * exists the auto choices show as "by target version" (the collation and the zero-date policy both
 * depend on the target's version, so a value cannot be promised yet); once `eff` is known it holds
 * exactly what the plan resolved.
 */
export function mysqlDefaultsLine(o: MysqlCopyOptions, eff?: CopyEffective): string {
  const charset = eff?.charset ?? o.charset ?? 'utf8mb4';
  const collation = o.collation === 'legacy' ? cs('collationLegacy') : (eff?.collation ?? o.collation ?? cs('versionDefault'));
  const engine = eff?.engine ?? (o.engine === 'auto' || o.engine === 'innodb' ? 'InnoDB' : o.engine === 'myisam' ? 'MyISAM' : cs('engine.keep'));
  const zero = eff?.zeroDates ? cs(k(`zero.${eff.zeroDates}`)) : o.zeroDates === 'auto' ? cs('versionDefault') : cs(k(`zero.${o.zeroDates}`));
  return [charset, collation, engine, cs('zeroDatesFact', { policy: zero })].join(' · ');
}

/**
 * A labelled row of 고급: the label is centred on a 26px control (`pt-1`) or, next to text
 * such as a radio list (`text`), sits on its first line.
 */
function Row({ label, children, align = 'control' }: { label: string; children: ReactNode; align?: 'control' | 'text' }) {
  return (
    <div className="grid grid-cols-[216px_minmax(0,1fr)] items-start gap-3">
      <div className={cn('text-right text-fg-secondary', align === 'control' && 'pt-1')}>{label}</div>
      <div className="min-w-0">{children}</div>
    </div>
  );
}

function RenameList({ options, pg, patch }: { options: CopyOptions; pg: boolean; patch: (p: Partial<CopyOptions>) => void }) {
  const set = (rename: NameMapping[]) => patch({ rename });
  const update = (i: number, m: Partial<NameMapping>) => set(options.rename.map((x, j) => (j === i ? { ...x, ...m } : x)));
  return (
    <div className="flex flex-col gap-1.5">
      {options.rename.map((m, i) => (
        <div key={i} className="flex items-center gap-1.5">
          <Select
            size="sm"
            className="w-24"
            aria-label={cs('rename')}
            value={m.kind}
            onChange={(kind) => update(i, { kind })}
            options={[
              // Schema renames only apply on PostgreSQL (`target_schema_name`); offering the kind on
              // MySQL / SQLite would silently do nothing.
              ...(pg ? [{ value: 'schema' as const, label: cs('renameKind.schema') }] : []),
              { value: 'table' as const, label: cs('renameKind.table') },
            ]}
          />
          {pg && m.kind === 'table' && (
            <Input dense className="w-28" aria-label={cs('renameSchema')} placeholder={cs('renameSchema')} value={m.schema ?? ''} onChange={(e) => update(i, { schema: e.target.value || undefined })} />
          )}
          <Input dense className="w-36" aria-label={cs('renameFrom')} placeholder={cs('renameFrom')} value={m.from} onChange={(e) => update(i, { from: e.target.value })} />
          <span className="text-fg-tertiary">→</span>
          <Input dense className="w-36" aria-label={cs('renameTo')} placeholder={cs('renameTo')} value={m.to} onChange={(e) => update(i, { to: e.target.value })} />
          <IconButton label={cs('renameRemove')} size="sm" onClick={() => set(options.rename.filter((_, j) => j !== i))}>
            <X size={12} />
          </IconButton>
        </div>
      ))}
      <Button size="sm" variant="ghost" className="-ml-2 w-fit" icon={<Plus size={12} />} onClick={() => set([...options.rename, { kind: pg ? 'schema' : 'table', from: '', to: '' }])}>
        {cs('renameAdd')}
      </Button>
    </div>
  );
}

export function OptionsStep() {
  const uid = useId();
  const draft = useCopyStore((s) => s.wizard.draft);
  const plan = useCopyStore((s) => s.wizard.plan);
  const o = draft.options;
  const conn = useConnectionsStore((s) => s.connections.find((c) => c.id === draft.source.connId));
  const family = familyOf(conn?.driver);
  const [advanced, setAdvanced] = useState(false);
  const patch = (p: Partial<CopyOptions>) => useCopyStore.getState().setDraft({ options: { ...o, ...p } });
  const patchMysql = (p: Partial<MysqlCopyOptions>) => patch({ mysql: { ...o.mysql, ...p } });
  const patchPg = (p: Partial<PgCopyOptions>) => patch({ pg: { ...o.pg, ...p } });
  const sqlite = family === 'sqlite';
  const conc = sqlite ? 1 : o.tableConcurrency;
  // Checksum verification opens one more session on each side (spec 4.1, `sessions()` in core).
  const verifySessions = o.verify === 'checksum' ? 1 : 0;
  // utf8mb4 collations are meaningless under another character set: offer only 자동 / legacy then.
  const collations = (o.mysql.charset ?? 'utf8mb4') === 'utf8mb4' ? COLLATIONS : [];

  return (
    <div className="flex flex-col gap-4">
      <section className="flex flex-col gap-2">
        <Caption>{cs('existing')}</Caption>
        <RadioList
          name="existing"
          label={cs('existing')}
          value={o.existing}
          onChange={(existing) => patch({ existing })}
          options={POLICIES.map((p) => ({ value: p, label: cs(k(`existing.${p}`)), help: cs(k(`existingHelp.${p}`)), danger: p === 'truncate' || p === 'drop_recreate' }))}
        />
      </section>
      <div className="grid grid-cols-2 gap-6">
        <section className="flex flex-col gap-2">
          <Caption>{cs('content')}</Caption>
          <RadioList
            inline
            name="content"
            label={cs('content')}
            value={o.content}
            onChange={(content) => patch({ content })}
            options={(['both', 'schema_only', 'data_only'] as const).map((v) => ({ value: v, label: cs(k(`content.${v}`)) }))}
          />
        </section>
        <section className="flex flex-col gap-2">
          <Caption>{cs('verify')}</Caption>
          <RadioList
            inline
            name="verify"
            label={cs('verify')}
            value={o.verify}
            onChange={(verify) => patch({ verify })}
            options={(['none', 'row_count', 'checksum'] as const).map((v) => ({ value: v, label: cs(k(`verify.${v}`)) }))}
          />
          {o.verify === 'checksum' && <div className="text-sm text-fg-secondary">{cs('checksumHelp')}</div>}
        </section>
      </div>
      {family === 'mysql' && (
        <div className="text-sm text-fg-secondary" data-testid="mysql-defaults">
          <span className="font-semibold">{cs('mysqlDefaults')}</span> · {mysqlDefaultsLine(o.mysql, plan?.effective)}
        </div>
      )}

      <Disclosure open={advanced} onToggle={() => setAdvanced((v) => !v)} title={cs('advanced')}>
        {family === 'mysql' && (
          <>
            <Row label={cs('charset')}>
              <Select
                aria-label={cs('charset')}
                className="w-72"
                value={o.mysql.charset ?? AUTO}
                onChange={(v) => {
                  const charset = v === AUTO ? undefined : v;
                  // A utf8mb4 collation cannot stand under another character set: drop it back to 자동.
                  const stale = (charset ?? 'utf8mb4') !== 'utf8mb4' && !!o.mysql.collation && o.mysql.collation !== 'legacy';
                  patchMysql({ charset, ...(stale ? { collation: undefined } : {}) });
                }}
                options={[{ value: AUTO, label: cs('charsetAuto') }, ...CHARSETS.map((c) => ({ value: c, label: c }))]}
              />
            </Row>
            <Row label={cs('collation')}>
              <Select
                aria-label={cs('collation')}
                className="w-72"
                value={o.mysql.collation ?? AUTO}
                onChange={(v) => patchMysql({ collation: v === AUTO ? undefined : v })}
                options={[{ value: AUTO, label: cs('collationAuto') }, { value: 'legacy', label: cs('collationLegacy') }, ...collations.map((c) => ({ value: c, label: c }))]}
              />
            </Row>
            <Row label={cs('engine')} align="text">
              <RadioList
                inline
                name="engine"
                label={cs('engine')}
                value={o.mysql.engine}
                onChange={(engine) => patchMysql({ engine })}
                options={(['auto', 'keep', 'innodb', 'myisam'] as const).map((v) => ({ value: v, label: cs(k(`engine.${v}`)) }))}
              />
            </Row>
            <Row label={cs('zeroDates')} align="text">
              <RadioList
                name="zero"
                label={cs('zeroDates')}
                value={o.mysql.zeroDates}
                onChange={(zeroDates) => patchMysql({ zeroDates })}
                options={(['auto', 'null', 'keep', 'min'] as const).map((v) => ({ value: v, label: cs(k(`zero.${v}`)), help: cs(k(`zeroHelp.${v}`)) }))}
              />
            </Row>
            <Row label={cs('keepDefiner')}>
              <Switch checked={o.mysql.keepDefiner} onChange={(keepDefiner) => patchMysql({ keepDefiner })} aria-label={cs('keepDefiner')} className="mt-1" />
            </Row>
            <Row label={cs('unmappable')} align="text">
              <RadioList
                inline
                name="unmappable"
                label={cs('unmappable')}
                value={o.onUnmappable}
                onChange={(onUnmappable) => patch({ onUnmappable })}
                options={(['replace', 'fail'] as const).map((v) => ({ value: v, label: cs(k(`unmappable.${v}`)) }))}
              />
            </Row>
          </>
        )}
        {family === 'postgres' && (
          <>
            <Row label={cs('pgCollationFallback')}>
              <Switch checked={o.pg.collationFallback} onChange={(collationFallback) => patchPg({ collationFallback })} aria-label={cs('pgCollationFallback')} className="mt-1" />
            </Row>
            <Row label={cs('pgSourceEncoding')}>
              <Select
                aria-label={cs('pgSourceEncoding')}
                className="w-56"
                value={o.pg.sourceEncoding ?? AUTO}
                onChange={(v) => patchPg({ sourceEncoding: v === AUTO ? undefined : (v as PgCopyOptions['sourceEncoding']) })}
                options={[
                  { value: AUTO, label: cs('pgSourceEncodingNone') },
                  { value: 'euc_kr', label: 'EUC-KR' },
                  { value: 'latin1', label: 'LATIN1' },
                  { value: 'utf8', label: 'UTF-8' },
                ]}
              />
              <div className="mt-1 text-sm text-fg-secondary">{cs('pgSourceEncodingHelp')}</div>
            </Row>
          </>
        )}
        <Row label={cs('lowercaseNames')}>
          <Switch checked={o.nameCase === 'lower'} onChange={(on) => patch({ nameCase: on ? 'lower' : 'keep' })} aria-label={cs('lowercaseNames')} className="mt-1" />
        </Row>
        <Row label={cs('rename')}>
          <RenameList options={o} pg={family === 'postgres'} patch={patch} />
        </Row>
        <Row label={cs('indexesAfterData')}>
          <Switch checked={o.indexesAfterData} onChange={(indexesAfterData) => patch({ indexesAfterData })} aria-label={cs('indexesAfterData')} className="mt-1" />
        </Row>
        <Row label={cs('concurrency')} align="text">
          <RadioList
            inline
            name="concurrency"
            label={cs('concurrency')}
            value={String(conc) as '1' | '2' | '3' | '4'}
            onChange={(v) => patch({ tableConcurrency: Number(v) })}
            options={(['1', '2', '3', '4'] as const).map((v) => ({ value: v, label: v, disabled: sqlite && v !== '1' }))}
          />
          <div className="mt-1 text-sm text-fg-secondary">{sqlite ? cs('concurrencySqlite') : cs('concurrencyHelp', { n: conc + 1 + verifySessions, m: conc + 2 + verifySessions })}</div>
        </Row>
        <Row label={cs('batchRows')}>
          <NumberInput
            id={`${uid}-batch`}
            aria-label={cs('batchRows')}
            className="w-32"
            integer
            min={1}
            max={65536}
            placeholder="50000"
            value={o.batchRows}
            onChange={(batchRows) => patch({ batchRows })}
          />
        </Row>
        <Row label={cs('lockTimeout')}>
          <NumberInput
            id={`${uid}-lock`}
            aria-label={cs('lockTimeout')}
            className="w-32"
            integer
            min={1}
            max={600}
            placeholder="5"
            value={o.targetLockTimeoutS}
            onChange={(targetLockTimeoutS) => patch({ targetLockTimeoutS })}
          />
        </Row>
        <Row label={cs('verifySkipped')}>
          <Switch checked={o.verifySkipped} onChange={(verifySkipped) => patch({ verifySkipped })} aria-label={cs('verifySkipped')} className="mt-1" />
        </Row>
      </Disclosure>
    </div>
  );
}
