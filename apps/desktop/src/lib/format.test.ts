import { describe, expect, it } from 'vitest';
import {
  baseName,
  formatByteRate,
  formatBytes,
  formatCompact,
  formatDuration,
  formatEta,
  formatInt,
  formatRowRate,
  joinPath,
  num,
  parseMemSize,
  percent,
} from './format';

describe('num', () => {
  it('normalises bigint and number, keeps undefined', () => {
    expect(num(BigInt(42))).toBe(42);
    expect(num(42)).toBe(42);
    expect(num(undefined)).toBeUndefined();
    expect(num(null)).toBeUndefined();
  });
});

describe('formatInt', () => {
  it('uses thousands separators and accepts bigint', () => {
    expect(formatInt(1234567)).toBe('1,234,567');
    expect(formatInt(BigInt(48_200_000))).toBe('48,200,000');
    expect(formatInt(0)).toBe('0');
    expect(formatInt(undefined)).toBe('\u2014');
  });
});

describe('formatCompact', () => {
  it('abbreviates large counts', () => {
    expect(formatCompact(999)).toBe('999');
    expect(formatCompact(1200)).toBe('1.2K');
    expect(formatCompact(BigInt(48_200_000))).toBe('48.2M');
    expect(formatCompact(1_250_000_000)).toBe('1.3B');
    expect(formatCompact(215_000_000)).toBe('215M');
  });
});

describe('formatBytes', () => {
  it('uses binary units', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(512)).toBe('512 B');
    expect(formatBytes(1536)).toBe('1.5 KB');
    expect(formatBytes(12.3 * 1024 ** 2)).toBe('12.3 MB');
    expect(formatBytes(BigInt(48 * 1024 ** 3), 0)).toBe('48 GB');
    expect(formatBytes(undefined)).toBe('\u2014');
  });
});

describe('parseMemSize', () => {
  it('parses PostgreSQL style sizes (kB default)', () => {
    expect(parseMemSize('64MB')).toBe(64 * 1024 ** 2);
    expect(parseMemSize('1 GB')).toBe(1024 ** 3);
    expect(parseMemSize('512kB')).toBe(512 * 1024);
    expect(parseMemSize('8192')).toBe(8192 * 1024);
    expect(parseMemSize('abc')).toBeUndefined();
    expect(parseMemSize(undefined)).toBeUndefined();
  });
});

describe('durations and rates', () => {
  it('formats durations at the right granularity', () => {
    expect(formatDuration(800)).toBe('0.8s');
    expect(formatDuration(12_000)).toBe('12s');
    expect(formatDuration(65_000)).toBe('1m 05s');
    expect(formatDuration(BigInt(2 * 3600_000 + 3 * 60_000))).toBe('2h 03m');
    expect(formatDuration(-1)).toBe('\u2014');
  });
  it('formats ETA and rates', () => {
    expect(formatEta(65)).toBe('~1m 05s');
    expect(formatEta(undefined)).toBe('\u2014');
    expect(formatByteRate(12.3 * 1024 ** 2)).toBe('12.3 MB/s');
    expect(formatRowRate(45_200)).toBe('45.2K rows/s');
  });
  it('computes a clamped percent', () => {
    expect(percent(50, 200)).toBe(25);
    expect(percent(500, 200)).toBe(100);
    expect(percent(5, 0)).toBeUndefined();
    expect(percent(5, undefined)).toBeUndefined();
  });
});

describe('paths', () => {
  it('handles both separators', () => {
    expect(baseName('C:\\exports\\a.csv')).toBe('a.csv');
    expect(baseName('/tmp/a.csv')).toBe('a.csv');
    expect(joinPath('C:\\exports', 'a.csv')).toBe('C:\\exports\\a.csv');
    expect(joinPath('/tmp/', 'a.csv')).toBe('/tmp/a.csv');
    expect(joinPath('', 'a.csv')).toBe('a.csv');
  });
});
