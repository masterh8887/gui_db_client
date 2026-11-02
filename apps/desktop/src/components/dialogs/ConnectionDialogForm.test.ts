import { describe, expect, it } from 'vitest';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { emptyForm, fromConnection, toInput, withDriver, withoutSecrets } from './ConnectionDialogForm';

const drivers = mockDrivers();
const pg = drivers.find((d) => d.id === 'postgres')!;
const my = drivers.find((d) => d.id === 'mysql')!;
const [prod] = mockConnections();

describe('connection form model', () => {
  it('keeps the tag, and omits it (= clears it in core) when untagged', () => {
    const f = fromConnection(prod);
    expect(f.tag).toBe('production');
    expect(toInput(f, prod, pg).tag).toBe('production');
    const cleared = toInput({ ...f, tag: undefined }, prod, pg);
    expect(cleared).not.toHaveProperty('tag');
  });

  it('never sends a secret together with its clear flag, nor an ask-every-time password', () => {
    const f = { ...fromConnection(prod), password: 'typed', clearPassword: true };
    const input = toInput(f, prod, pg);
    expect(input.password).toBeUndefined();
    expect(input.clearPassword).toBe(true);
    const askEveryTime = toInput({ ...fromConnection(prod), password: 'once', savePassword: false }, prod, pg);
    expect(askEveryTime.password).toBeUndefined();
    expect(withoutSecrets({ ...f, sshPassword: 'x' })).toMatchObject({ password: '', sshPassword: '', clearPassword: false });
  });

  it('switching drivers fills defaults and moves a default port, but keeps a typed one', () => {
    const f = emptyForm(pg);
    expect(f.port).toBe('5432');
    expect(withDriver(f, my, pg)).toMatchObject({ driver: 'mysql', port: '3306', fields: { sslmode: 'prefer' } });
    expect(withDriver({ ...f, port: '6543' }, my, pg).port).toBe('6543');
  });

  it('a new connection has an empty id (the backend assigns it) and only the driver fields', () => {
    const f = { ...emptyForm(pg), name: ' x ', fields: { sslmode: 'require', stray: 'y' } };
    const input = toInput(f, undefined, pg);
    expect(input).toMatchObject({ id: '', name: 'x', driver: 'postgres', port: 5432, fields: { sslmode: 'require' } });
    expect(input.fields).not.toHaveProperty('stray');
    expect(input.ssh).toBeUndefined();
  });

  it('never sends writeAllowed: only the copy wizard and the CLI change it (docs/db-copy.md 8)', () => {
    const conn = { ...mockConnections().find((c) => c.id === 'c-staging')!, writeAllowed: true };
    const driver = mockDrivers().find((d) => d.id === conn.driver);
    const input = toInput(fromConnection(conn, driver), conn, driver);
    expect('writeAllowed' in input).toBe(false);
  });
});
