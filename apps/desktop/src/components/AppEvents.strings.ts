// Strings of the event / bootstrap wiring (log lines only).

import { defineStrings } from '@/lib/i18n';

export const es = defineStrings('appEvents', {
  en: {
    loadFailed: 'Failed to load {what}: {error}',
    connections: 'connections',
    settings: 'settings',
    logs: 'logs',
    jobs: 'jobs',
    copyComplete: 'Copy complete: {label}',
    copyFailed: 'Copy failed: {label}',
    copyCancelled: 'Copy cancelled: {label}',
    exportComplete: 'Export complete: {label}',
    exportFailed: 'Export failed: {label}',
    exportCancelled: 'Export cancelled: {label}',
  },
  ko: {
    loadFailed: '{what}을(를) 불러오지 못했습니다: {error}',
    connections: '연결 목록',
    settings: '설정',
    logs: '로그',
    jobs: '작업 목록',
    copyComplete: '복사 완료: {label}',
    copyFailed: '복사 실패: {label}',
    copyCancelled: '복사 취소됨: {label}',
    exportComplete: '추출 완료: {label}',
    exportFailed: '추출 실패: {label}',
    exportCancelled: '추출 취소됨: {label}',
  },
});
