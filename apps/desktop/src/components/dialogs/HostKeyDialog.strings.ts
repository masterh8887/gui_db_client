// Strings of the SSH host-key prompt (DESIGN.md §5.I "Prompts").

import { defineStrings } from '@/lib/i18n';

export const hs = defineStrings('hostKey', {
  en: {
    title: 'Verify host key',
    body: 'The host key of {host} is not known yet. Compare the fingerprint before accepting.',
    changed: 'The host key of {host} differs from the one seen before. Someone could be intercepting the connection: accept only if the server key really changed.',
    host: 'Host',
    keyType: 'Key type',
    fingerprint: 'Fingerprint',
    message: 'Message from ssh',
    accept: 'Accept',
    reject: 'Reject',
    more: '{n} more waiting',
  },
  ko: {
    title: '호스트 키 확인',
    body: '{host}의 호스트 키를 처음 봅니다. 지문을 확인한 뒤 수락하세요.',
    changed: '{host}의 호스트 키가 이전과 다릅니다. 연결이 가로채이고 있을 수 있으니 서버 키가 실제로 바뀐 경우에만 수락하세요.',
    host: '호스트',
    keyType: '키 종류',
    fingerprint: '지문',
    message: 'ssh 메시지',
    accept: '수락',
    reject: '거부',
    more: '대기 중 {n}건',
  },
});
