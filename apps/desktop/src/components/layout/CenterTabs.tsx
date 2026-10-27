'use client';

import { ExtractTab } from '@/components/extract/ExtractTab';
import { HistoryTab } from '@/components/history/HistoryTab';
import { TableTab } from '@/components/table/TableTab';
import { Tabs, type TabItem } from '@/components/ui';
import { t } from '@/lib/i18n';
import { tableLabel } from '@/lib/utils';
import { useUiStore, type CenterTab } from '@/stores/ui';

export function CenterTabs() {
  const centerTab = useUiStore((s) => s.centerTab);
  const setCenterTab = useUiStore((s) => s.setCenterTab);
  const selectedTable = useUiStore((s) => s.selectedTable);

  const items: TabItem<CenterTab>[] = [
    { value: 'extract', label: t('tab.extract') },
    ...(selectedTable
      ? [{ value: 'table' as const, label: `${t('tab.table')}: ${tableLabel(selectedTable.ref)}` }]
      : []),
    { value: 'history', label: t('tab.history') },
  ];
  const value: CenterTab = centerTab === 'table' && !selectedTable ? 'extract' : centerTab;

  return (
    <Tabs value={value} onChange={setCenterTab} items={items}>
      {(v) => (v === 'extract' ? <ExtractTab /> : v === 'table' ? <TableTab /> : <HistoryTab />)}
    </Tabs>
  );
}
