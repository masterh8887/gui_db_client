// Korean / English strings for the Script tab.

const STRINGS = {
  runtimeNode: 'Node.js',
  runtimePython: 'Python',
  runtimeBash: 'Bash',
  notInstalled: '미설치',
  install: '설치',
  installing: '설치 중…',
  detecting: '확인 중…',
  run: '▶ 실행',
  stop: '■ 중지',
  clearLog: '지우기',
  stateIdle: '준비',
  stateDone: '완료',
  stateFailed: '실패',
  stateCancelled: '취소됨',
  stateRunning: '실행 중',
  stateInstall: '설치 중',
  exitCode: (code: number) => `exit ${code}`,
  duration: (ms: number) => ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`,
  placeholderNode: `// Node.js 스크립트
// BULKDB_HOST, BULKDB_PORT, BULKDB_DATABASE, BULKDB_USER, BULKDB_PASSWORD 환경변수 사용 가능
const { execSync } = require('child_process');
console.log('host:', process.env.BULKDB_HOST);`,
  placeholderPython: `# Python 스크립트
# BULKDB_HOST, BULKDB_PORT, BULKDB_DATABASE, BULKDB_USER, BULKDB_PASSWORD 환경변수 사용 가능
import os, sys
print('host:', os.environ.get('BULKDB_HOST'))`,
  placeholderBash: `#!/usr/bin/env bash
# Bash 스크립트 — BULKDB_* 환경변수 사용 가능
echo "host: $BULKDB_HOST"`,
  installPrompt: (name: string) => `${name} 런타임이 설치되어 있지 않습니다. 설치하시겠습니까?`,
};

export function ss<K extends keyof typeof STRINGS>(key: K, ...args: (typeof STRINGS)[K] extends (...a: infer A) => unknown ? A : []): string {
  const v = STRINGS[key];
  return typeof v === 'function' ? (v as (...a: unknown[]) => string)(...args) : (v as string);
}
