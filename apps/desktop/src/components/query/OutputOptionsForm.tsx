'use client';

// Export panel 출력 (DESIGN.md §5.D): the name (feeds `{name}` in the file pattern) and every
// output option of the doc (folder, pattern, format, CSV options, compression, Parquet options,
// split rows / MB, overwrite) plus the Excel preset. One column, two from 900px of panel width.
// CSV encoding (docs/mysql-compat.md §6): UTF-8 (default, no `encoding` key) or EUC-KR (CP949);
// EUC-KR disables the BOM (UTF-8 only); Parquet is always UTF-8.

import type { Compression, CsvEncoding, OutputFormat, ParquetCompression } from 'ipc-types';
import { useConnectionsStore } from '@/stores/connections';
import { FileSpreadsheet, FolderOpen } from 'lucide-react';
import { useId } from 'react';
import { pickDirectory } from '@/lib/dialogs';
import { num } from '@/lib/format';
import { cn } from '@/lib/utils';
import { Button, Checkbox, IconButton, Input, NumberInput, SegmentedControl, Select } from '@/components/ui';
import { csvEncoding, defaultCsvOpts, defaultParquetOpts, defaultXlsxOpts, useExtractStore, useQueryDoc } from '@/stores/extract';
import { FormRow } from './parts';
import { qs } from './strings';

type CompressChoice = 'none' | Compression;

function CheckRow({
  id,
  label,
  checked,
  onChange,
  disabled,
  title,
}: {
  id: string;
  label: string;
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  title?: string;
}) {
  return (
    <label htmlFor={id} title={title} className={cn('flex min-h-(--h-control-sm) items-center gap-2 text-base', disabled && 'text-fg-tertiary')}>
      <Checkbox id={id} checked={checked} onChange={onChange} disabled={disabled} />
      <span>{label}</span>
    </label>
  );
}

const encodingOptions = (disabled: boolean) => [
  { value: 'utf-8' as const, label: qs('encodingUtf8'), disabled },
  { value: 'euc-kr' as const, label: qs('encodingEucKr'), disabled },
];

export function OutputOptionsForm({ tabId }: { tabId: string }) {
  const uid = useId();
  const id = (k: string) => `${uid}-${k}`;
  const doc = useQueryDoc(tabId, (d) => ({ output: d.output, name: d.name, connId: d.connId }));
  if (!doc) return null;
  const { output, name, connId } = doc;
  const ex = useExtractStore.getState;
  const csv = output.csv ?? defaultCsvOpts();
  const parquet = output.parquet ?? defaultParquetOpts();
  const xlsx = output.xlsx ?? defaultXlsxOpts();
  const encoding = csvEncoding(csv);
  const eucKr = encoding === 'euc-kr';
  const conn = useConnectionsStore.getState().connections.find((c) => c.id === connId);
  const isPg = conn?.driver === 'postgres';

  const browse = async () => {
    const dir = await pickDirectory(output.dir);
    if (dir) ex().setOutput(tabId, { dir });
  };

  return (
    <div className="@container" aria-label={qs('output')} role="group">
      <div className="grid grid-cols-1 gap-x-8 gap-y-2 @min-[900px]:grid-cols-2">
        {/* Column 1: what and where. */}
        <div className="flex flex-col gap-2">
          <FormRow label={qs('nameLabel')} htmlFor={id('name')}>
            <Input id={id('name')} value={name} placeholder={qs('namePlaceholder')} onChange={(e) => ex().setName(tabId, e.target.value)} />
          </FormRow>
          <FormRow label={qs('dir')} htmlFor={id('dir')}>
            <div className="flex gap-1">
              <Input id={id('dir')} value={output.dir} placeholder={qs('dirPlaceholder')} onChange={(e) => ex().setOutput(tabId, { dir: e.target.value })} />
              <IconButton label={qs('browse')} onClick={() => void browse()}>
                <FolderOpen size={14} strokeWidth={1.75} />
              </IconButton>
            </div>
          </FormRow>
          <FormRow label={qs('pattern')} htmlFor={id('pattern')} hint={qs('patternHint')}>
            <Input id={id('pattern')} value={output.namePattern} onChange={(e) => ex().setOutput(tabId, { namePattern: e.target.value })} />
          </FormRow>
          <FormRow label={qs('format')}>
            <div className="flex items-center gap-3">
              <SegmentedControl<OutputFormat>
                aria-label={qs('format')}
                value={output.format}
                onChange={(format) => ex().setOutput(tabId, { format })}
                options={[
                  { value: 'csv', label: 'CSV' },
                  { value: 'parquet', label: 'Parquet' },
                  { value: 'xlsx', label: 'XLSX', disabled: isPg },
                ]}
              />
            </div>
          </FormRow>
          <FormRow label={qs('split')} htmlFor={id('split-rows')}>
            <div className="grid grid-cols-2 gap-2">
              <NumberInput
                id={id('split-rows')}
                aria-label={`${qs('split')}: ${qs('splitRows')}`}
                placeholder={qs('splitRows')}
                min={1}
                value={num(output.split?.rows)}
                onChange={(v) =>
                  ex().setSplit(
                    tabId,
                    v === undefined && !output.split?.megabytes ? undefined : { rows: v === undefined ? undefined : BigInt(v), megabytes: output.split?.megabytes },
                  )
                }
              />
              <NumberInput
                id={id('split-mb')}
                aria-label={`${qs('split')}: ${qs('splitMb')}`}
                placeholder={qs('splitMb')}
                min={1}
                value={num(output.split?.megabytes)}
                onChange={(v) =>
                  ex().setSplit(
                    tabId,
                    v === undefined && !output.split?.rows ? undefined : { rows: output.split?.rows, megabytes: v === undefined ? undefined : BigInt(v) },
                  )
                }
              />
            </div>
          </FormRow>
        </div>

        {/* Column 2: how the file is written. */}
        <div className="flex flex-col gap-2">
          {output.format === 'csv' ? (
            <>
              <FormRow label={qs('encoding')} hint={eucKr ? qs('encodingHint') : undefined}>
                <div>
                  <SegmentedControl<CsvEncoding>
                    aria-label={qs('encoding')}
                    value={encoding}
                    onChange={(v) => ex().setCsvEncoding(tabId, v)}
                    options={encodingOptions(false)}
                  />
                </div>
              </FormRow>
              <FormRow label={qs('delimiter')} htmlFor={id('delim')}>
                <div className="grid grid-cols-[64px_auto_minmax(0,1fr)] items-center gap-2">
                  <Input
                    id={id('delim')}
                    value={csv.delimiter === '\t' ? '\\t' : csv.delimiter}
                    maxLength={2}
                    onChange={(e) => ex().setCsv(tabId, { delimiter: e.target.value === '\\t' ? '\t' : e.target.value.slice(0, 1) || ',' })}
                  />
                  <label htmlFor={id('null')} className="text-base text-fg-secondary">
                    {qs('nullAs')}
                  </label>
                  <Input id={id('null')} value={csv.nullAs} placeholder={qs('nullPlaceholder')} onChange={(e) => ex().setCsv(tabId, { nullAs: e.target.value })} />
                </div>
              </FormRow>
              <FormRow label={qs('compress')} htmlFor={id('compress')}>
                <Select<CompressChoice>
                  id={id('compress')}
                  value={output.compress ?? 'none'}
                  onChange={(v) => ex().setOutput(tabId, { compress: v === 'none' ? undefined : v })}
                  options={[
                    { value: 'none', label: qs('none') },
                    { value: 'gzip', label: 'gzip' },
                    { value: 'zstd', label: 'zstd' },
                  ]}
                />
              </FormRow>
              <FormRow label={qs('csvOptions')}>
                <div className="flex flex-wrap gap-x-4 gap-y-0.5 pt-0.5">
                  <CheckRow id={id('header')} label={qs('header')} checked={csv.header} onChange={(header) => ex().setCsv(tabId, { header })} />
                  <CheckRow
                    id={id('bom')}
                    label={qs('bom')}
                    checked={csv.bom && !eucKr}
                    disabled={eucKr}
                    title={eucKr ? qs('bomUtf8Only') : undefined}
                    onChange={(bom) => ex().setCsv(tabId, { bom })}
                  />
                  <CheckRow id={id('crlf')} label={qs('crlf')} checked={csv.crlf} onChange={(crlf) => ex().setCsv(tabId, { crlf })} />
                </div>
              </FormRow>
            </>
          ) : output.format === 'xlsx' ? (
            <>
              <FormRow label={qs('xlsxOptions')}>
                <div className="flex flex-wrap gap-x-4 gap-y-0.5 pt-0.5">
                  <CheckRow id={id('xlsx-header')} label={qs('header')} checked={xlsx.header} onChange={(header) => ex().setXlsx(tabId, { header })} />
                </div>
              </FormRow>
            </>
          ) : (
            <>
              <FormRow label={qs('encoding')} hint={qs('encodingParquet')}>
                <div>
                  <SegmentedControl<CsvEncoding> aria-label={qs('encoding')} value="utf-8" onChange={() => undefined} options={encodingOptions(true)} />
                </div>
              </FormRow>
              <FormRow label={qs('parquetCompression')} htmlFor={id('pq-compress')}>
                <Select<ParquetCompression>
                  id={id('pq-compress')}
                  value={parquet.compression}
                  onChange={(compression) => ex().setParquet(tabId, { compression })}
                  options={[
                    { value: 'zstd', label: 'zstd' },
                    { value: 'snappy', label: 'snappy' },
                    { value: 'none', label: qs('none') },
                  ]}
                />
              </FormRow>
              <FormRow label={qs('rowGroupMb')} htmlFor={id('rg')}>
                <NumberInput id={id('rg')} min={8} max={2048} value={parquet.rowGroupMb} onChange={(v) => ex().setParquet(tabId, { rowGroupMb: v ?? 128 })} />
              </FormRow>
            </>
          )}
          <FormRow label="">
            <CheckRow id={id('overwrite')} label={qs('overwrite')} checked={output.overwrite} onChange={(overwrite) => ex().setOutput(tabId, { overwrite })} />
          </FormRow>
          <FormRow label="" hint={qs('excelPresetHint')}>
            <div>
              <Button
                size="sm"
                variant="secondary"
                icon={<FileSpreadsheet size={14} strokeWidth={1.75} />}
                onClick={() => ex().applyExcelPreset(tabId)}
                title={qs('excelPresetHint')}
              >
                {qs('excelPreset')}
              </Button>
            </div>
          </FormRow>
        </div>
      </div>
    </div>
  );
}
