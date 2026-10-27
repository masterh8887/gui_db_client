'use client';

import type { Compression, OutputFormat, ParquetCompression } from 'ipc-types';
import { FileSpreadsheet, FolderOpen } from 'lucide-react';
import { pickDirectory } from '@/lib/dialogs';
import { num } from '@/lib/format';
import { t } from '@/lib/i18n';
import { Button, Checkbox, Field, IconButton, Input, NumberInput, Select } from '@/components/ui';
import { defaultCsvOpts, defaultParquetOpts, useExtractStore } from '@/stores/extract';

type CompressChoice = 'none' | Compression;

export function OutputOptionsForm() {
  const output = useExtractStore((s) => s.output);
  const setOutput = useExtractStore((s) => s.setOutput);
  const setCsv = useExtractStore((s) => s.setCsv);
  const setParquet = useExtractStore((s) => s.setParquet);
  const setSplit = useExtractStore((s) => s.setSplit);
  const applyExcelPreset = useExtractStore((s) => s.applyExcelPreset);
  const csv = output.csv ?? defaultCsvOpts();
  const parquet = output.parquet ?? defaultParquetOpts();

  const browse = async () => {
    const dir = await pickDirectory(output.dir);
    if (dir) setOutput({ dir });
  };

  return (
    <fieldset className="flex flex-col gap-2">
      <legend className="mb-1 flex w-full items-center text-xs font-semibold text-muted-foreground">
        {t('extract.output')}
      </legend>
      <Field label={t('extract.dir')} htmlFor="out-dir">
        <div className="flex gap-1">
          <Input id="out-dir" value={output.dir} onChange={(e) => setOutput({ dir: e.target.value })} placeholder="C:\exports" />
          <IconButton label={t('extract.browse')} onClick={() => void browse()}>
            <FolderOpen size={14} />
          </IconButton>
        </div>
      </Field>
      <Field label={t('extract.pattern')} htmlFor="out-pattern" hint={t('extract.patternHint')}>
        <Input id="out-pattern" value={output.namePattern} onChange={(e) => setOutput({ namePattern: e.target.value })} />
      </Field>
      <div className="grid grid-cols-2 gap-2">
        <Field label={t('extract.format')} htmlFor="out-format">
          <Select<OutputFormat>
            id="out-format"
            value={output.format}
            onChange={(format) => setOutput({ format })}
            options={[
              { value: 'csv', label: 'CSV' },
              { value: 'parquet', label: 'Parquet' },
            ]}
          />
        </Field>
        {output.format === 'csv' ? (
          <Field label={t('extract.compress')} htmlFor="out-compress">
            <Select<CompressChoice>
              id="out-compress"
              value={output.compress ?? 'none'}
              onChange={(v) => setOutput({ compress: v === 'none' ? undefined : v })}
              options={[
                { value: 'none', label: t('extract.none') },
                { value: 'gzip', label: 'gzip' },
                { value: 'zstd', label: 'zstd' },
              ]}
            />
          </Field>
        ) : (
          <Field label={t('extract.parquetCompression')} htmlFor="out-pq-compress">
            <Select<ParquetCompression>
              id="out-pq-compress"
              value={parquet.compression}
              onChange={(compression) => setParquet({ compression })}
              options={[
                { value: 'zstd', label: 'zstd' },
                { value: 'snappy', label: 'snappy' },
                { value: 'none', label: t('extract.none') },
              ]}
            />
          </Field>
        )}
      </div>
      {output.format === 'csv' ? (
        <>
          <div className="grid grid-cols-2 gap-2">
            <Field label={t('extract.delimiter')} htmlFor="out-delim">
              <Input id="out-delim" value={csv.delimiter === '\t' ? '\\t' : csv.delimiter} maxLength={2} onChange={(e) => setCsv({ delimiter: e.target.value === '\\t' ? '\t' : e.target.value.slice(0, 1) || ',' })} />
            </Field>
            <Field label={t('extract.nullAs')} htmlFor="out-null">
              <Input id="out-null" value={csv.nullAs} placeholder="(empty)" onChange={(e) => setCsv({ nullAs: e.target.value })} />
            </Field>
          </div>
          <div className="grid grid-cols-2 gap-x-2">
            <Field inline label={t('extract.header')} htmlFor="out-header">
              <Checkbox id="out-header" checked={csv.header} onChange={(header) => setCsv({ header })} />
            </Field>
            <Field inline label={t('extract.bom')} htmlFor="out-bom">
              <Checkbox id="out-bom" checked={csv.bom} onChange={(bom) => setCsv({ bom })} />
            </Field>
            <Field inline label={t('extract.crlf')} htmlFor="out-crlf">
              <Checkbox id="out-crlf" checked={csv.crlf} onChange={(crlf) => setCsv({ crlf })} />
            </Field>
            <Field inline label={t('extract.overwrite')} htmlFor="out-overwrite">
              <Checkbox id="out-overwrite" checked={output.overwrite} onChange={(overwrite) => setOutput({ overwrite })} />
            </Field>
          </div>
        </>
      ) : (
        <div className="grid grid-cols-2 gap-2">
          <Field label={t('extract.rowGroupMb')} htmlFor="out-rg">
            <NumberInput id="out-rg" min={8} max={2048} value={parquet.rowGroupMb} onChange={(v) => setParquet({ rowGroupMb: v ?? 128 })} />
          </Field>
          <Field inline label={t('extract.overwrite')} htmlFor="out-overwrite-pq" className="mt-4">
            <Checkbox id="out-overwrite-pq" checked={output.overwrite} onChange={(overwrite) => setOutput({ overwrite })} />
          </Field>
        </div>
      )}
      <div className="grid grid-cols-2 gap-2">
        <Field label={`${t('extract.split')}: ${t('extract.splitRows')}`} htmlFor="out-split-rows">
          <NumberInput
            id="out-split-rows"
            min={1}
            value={num(output.split?.rows)}
            onChange={(v) => setSplit(v === undefined && !output.split?.megabytes ? undefined : { rows: v === undefined ? undefined : BigInt(v), megabytes: output.split?.megabytes })}
          />
        </Field>
        <Field label={`${t('extract.split')}: ${t('extract.splitMb')}`} htmlFor="out-split-mb">
          <NumberInput
            id="out-split-mb"
            min={1}
            value={num(output.split?.megabytes)}
            onChange={(v) => setSplit(v === undefined && !output.split?.rows ? undefined : { rows: output.split?.rows, megabytes: v === undefined ? undefined : BigInt(v) })}
          />
        </Field>
      </div>
      <div>
        <Button size="sm" variant="outline" icon={<FileSpreadsheet size={13} />} onClick={applyExcelPreset} title={t('extract.excelPresetHint')}>
          {t('extract.excelPreset')}
        </Button>
      </div>
    </fieldset>
  );
}
