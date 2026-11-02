// Regression tests for the copy-ui "beyond cap" findings that are pure logic or the browser mock:
//  - the discard-confirm dirtiness check (isDirtyDraft)
//  - the empty table-selection gate (selectionEmpty)
//  - dropping incomplete rename rows (cleanRenames)
//  - a warned job requiring only the still-missing acks (continueBlock + pendingAckKeys)
//  - the MySQL defaults line before/after a plan (mysqlDefaultsLine)
//  - the mock effective session count with checksum verification.

import type { CopyPlan, CopyRequest, MysqlCopyOptions } from 'ipc-types';
import { beforeEach, describe, expect, it } from 'vitest';
import { copyPlan } from '@/lib/commands';
import { LEGACY_CONN_ID, MYSQL8_CONN_ID } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { DEFAULT_COPY_OPTIONS } from '@/stores/copy';
import { cleanRenames, continueBlock, isDirtyDraft, pendingAckKeys, selectionEmpty } from './model';
import { mysqlDefaultsLine } from './OptionsStep';
import { cs } from './strings';

const opts = () => structuredClone(DEFAULT_COPY_OPTIONS);
const draft = (patch: Partial<CopyRequest> = {}): CopyRequest => ({
  source: { connId: 'c-prod', database: 'orders' },
  target: { connId: 'c-staging', database: 'analytics_dw' },
  selection: { kind: 'all' },
  options: opts(),
  ...patch,
});

describe('copy-ui beyond-cap logic', () => {
  describe('isDirtyDraft (entry 35)', () => {
    it('is false for a fresh draft and true once tables, options or renames change', () => {
      expect(isDirtyDraft(draft())).toBe(false);
      expect(isDirtyDraft(draft({ selection: { kind: 'tables', tables: [{ name: 't' }] } }))).toBe(true);
      expect(isDirtyDraft(draft({ options: { ...opts(), existing: 'append' } }))).toBe(true);
      expect(isDirtyDraft(draft({ options: { ...opts(), rename: [{ kind: 'table', from: 'a', to: 'b' }] } }))).toBe(true);
      // Choosing endpoints alone is not a reason to confirm.
      expect(isDirtyDraft(draft({ target: { connId: 'c-other', database: 'x' } }))).toBe(false);
    });
  });

  describe('selectionEmpty (entry 29)', () => {
    it('is true only for a tables selection with nothing chosen', () => {
      expect(selectionEmpty(draft())).toBe(false);
      expect(selectionEmpty(draft({ selection: { kind: 'tables', tables: [] } }))).toBe(true);
      expect(selectionEmpty(draft({ selection: { kind: 'tables', tables: [{ name: 't' }] } }))).toBe(false);
    });
  });

  describe('cleanRenames (entry 27)', () => {
    it('drops rows with an empty or whitespace from/to', () => {
      const o = {
        ...opts(),
        rename: [
          { kind: 'table' as const, from: 'a', to: 'b' },
          { kind: 'table' as const, from: '', to: '' },
          { kind: 'table' as const, from: 'c', to: '' },
          { kind: 'table' as const, from: '', to: 'd' },
          { kind: 'table' as const, from: '  ', to: 'e' },
        ],
      };
      expect(cleanRenames(o).rename).toEqual([{ kind: 'table', from: 'a', to: 'b' }]);
    });
    it('returns the same object when every row is complete', () => {
      const o = { ...opts(), rename: [{ kind: 'table' as const, from: 'a', to: 'b' }] };
      expect(cleanRenames(o)).toBe(o);
    });
  });

  describe('continueBlock / pendingAckKeys (entry 40)', () => {
    const plan = { blockers: [], acks: [{ code: 'A', key: 'A', severity: 'warning', items: [] }, { code: 'B', key: 'B', severity: 'warning', items: [] }] } as unknown as CopyPlan;

    it('requires every ack when the pending set is unknown', () => {
      expect(continueBlock(plan, [])).toBe('acks');
      expect(continueBlock(plan, ['A'])).toBe('acks');
      expect(continueBlock(plan, ['A', 'B'])).toBeNull();
    });

    it('requires only the still-missing keys, so acks given at start are not re-ticked', () => {
      const pending = pendingAckKeys([{ code: 'B' }]);
      // A was acknowledged at start (not pending); only B is still missing.
      expect(continueBlock(plan, [], pending)).toBe('acks');
      expect(continueBlock(plan, ['B'], pending)).toBeNull();
    });

    it('pendingAckKeys reads one key per preflight warning, undefined when unknown', () => {
      expect(pendingAckKeys(undefined)).toBeUndefined();
      expect([...pendingAckKeys([{ code: 'X' }, { code: 'Y' }])!].sort()).toEqual(['X', 'Y']);
    });
  });

  describe('mysqlDefaultsLine (entry 28)', () => {
    const m = (): MysqlCopyOptions => opts().mysql;
    it('shows the version-dependent values as "by target version" before a plan exists', () => {
      const line = mysqlDefaultsLine(m());
      expect(line).toContain(cs('versionDefault'));
      expect(line).not.toContain('utf8mb4_0900_ai_ci');
    });
    it('uses the resolved effective values once a plan exists', () => {
      const line = mysqlDefaultsLine(m(), {
        charset: 'utf8mb4',
        collation: 'utf8mb4_0900_ai_ci',
        engine: 'InnoDB',
        zeroDates: 'null',
        loadMethod: 'mysql_insert',
        tableConcurrency: 1,
        consistentSnapshot: true,
        sourceSessions: 2,
        targetSessions: 3,
      });
      expect(line).toContain('utf8mb4_0900_ai_ci');
      expect(line).toContain('InnoDB');
    });
  });

  describe('effective session count (entry 25)', () => {
    beforeEach(() => {
      mockBackend.reset();
      mockBackend.configure({ latencyMs: 0 });
    });
    it('adds one session on each side for checksum verification', async () => {
      const src = { connId: LEGACY_CONN_ID, database: 'shop' };
      const tgt = { connId: MYSQL8_CONN_ID, database: 'shop_utf8' };
      const rowCount = await copyPlan({ source: src, target: tgt, selection: { kind: 'all' }, options: { ...opts(), verify: 'row_count' } });
      const checksum = await copyPlan({ source: src, target: tgt, selection: { kind: 'all' }, options: { ...opts(), verify: 'checksum' } });
      expect(checksum.effective.sourceSessions).toBe(rowCount.effective.sourceSessions + 1);
      expect(checksum.effective.targetSessions).toBe(rowCount.effective.targetSessions + 1);
    });
  });
});
