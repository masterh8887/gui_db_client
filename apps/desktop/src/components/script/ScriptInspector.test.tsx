import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { useConnectionsStore } from '@/stores/connections';
import { useScriptStore } from '@/stores/script';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { ScriptInspector } from './ScriptInspector';

const TAB = 'script-tab-1';

function setup() {
  const conns = mockConnections();
  const connId = conns[0].id;
  useConnectionsStore.setState({ connections: conns, drivers: mockDrivers(), loaded: true });
  useScriptStore.getState().createDoc(TAB, connId);
  // No editor is mounted in this test, so insertSnippet appends to the document.
  return { conns, connId };
}

const script = () => useScriptStore.getState().docs[TAB]?.script ?? '';

function renderInspector(connId: string) {
  render(
    <TooltipProvider>
      <ScriptInspector tabId={TAB} connId={connId} />
    </TooltipProvider>,
  );
}

describe('ScriptInspector', () => {
  beforeEach(() => {
    resetStores();
  });

  it('lists the saved connections and inserts bulkdb.db(name) on click', async () => {
    const user = userEvent.setup();
    const { conns, connId } = setup();
    renderInspector(connId);

    for (const c of conns) expect(screen.getByText(c.name)).toBeInTheDocument();

    await user.click(screen.getByText(conns[1].name));
    expect(script()).toContain(`const ${conns[1].name.replace(/[^A-Za-z0-9_$]+/g, '_').replace(/^_+|_+$/g, '')} = bulkdb.db('${conns[1].name}');`);
  });

  it('inserts a snippet from the catalog', async () => {
    const user = userEvent.setup();
    const { connId } = setup();
    renderInspector(connId);

    await user.click(screen.getByText('쿼리 빌더'));
    expect(script()).toContain("bulkdb.from('테이블')");
  });

  it('previews the export structure (columns only) and inserts matching code', async () => {
    const user = userEvent.setup();
    const { connId } = setup();
    renderInspector(connId);

    const columns = screen.getByLabelText('컬럼');
    await user.type(columns, 'id, name, total');
    // The structure preview shows the column headers, no data rows.
    const preview = screen.getByText('id').closest('div')!;
    expect(within(preview.parentElement!).getByText('name')).toBeInTheDocument();
    expect(within(preview.parentElement!).getByText('total')).toBeInTheDocument();
    expect(screen.queryByText(/데이터 행은 실행 시 채워집니다/)).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: '내보내기 코드 삽입' }));
    expect(script()).toContain("await bulkdb.toXlsx(rows, 'out.xlsx', { columns: ['id', 'name', 'total'] });");
  });

  it('builds a multi-sheet xlsx when a sheet is added', async () => {
    const user = userEvent.setup();
    const { connId } = setup();
    renderInspector(connId);

    await user.click(screen.getByRole('button', { name: /시트 추가/ }));
    await user.click(screen.getByRole('button', { name: '내보내기 코드 삽입' }));
    const code = script();
    expect(code).toContain('const rows1 = [];');
    expect(code).toContain('const rows2 = [];');
    expect(code).toContain("}, 'out.xlsx');");
  });
});
