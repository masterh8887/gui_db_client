// Strings of the SSH password / passphrase prompt (DESIGN.md §5.I "Prompts").

import { defineStrings } from '@/lib/i18n';

export const sp = defineStrings('sshPassword', {
  en: {
    title: 'SSH authentication',
    password: 'Password for {user}@{host}',
    passphrase: 'Passphrase for the key of {user}@{host}',
    hint: 'Used once for this tunnel; never written to disk.',
    cancel: 'Cancel',
    submit: 'OK',
  },
  ko: {
    title: 'SSH 인증',
    password: '{user}@{host} 비밀번호',
    passphrase: '{user}@{host} 키 암호',
    hint: '이 터널에 한 번만 사용되며 디스크에 저장되지 않습니다.',
    cancel: '취소',
    submit: '확인',
  },
});
