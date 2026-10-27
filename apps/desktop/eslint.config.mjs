import { defineConfig, globalIgnores } from 'eslint/config';
import nextVitals from 'eslint-config-next/core-web-vitals';
import nextTs from 'eslint-config-next/typescript';

export default defineConfig([
  ...nextVitals,
  ...nextTs,
  globalIgnores(['.next/**', 'out/**', 'src-tauri/**', 'node_modules/**', 'next-env.d.ts']),
  {
    rules: {
      // Static export: server-only APIs are design errors in this app.
      'no-restricted-imports': [
        'error',
        {
          paths: [
            { name: 'next/headers', message: 'No server features: this app is a static export.' },
            { name: 'next/server', message: 'No server features: this app is a static export.' },
          ],
        },
      ],
    },
  },
]);
