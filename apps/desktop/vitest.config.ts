import { defineConfig } from 'vitest/config';
import path from 'node:path';

export default defineConfig({
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./src/test/setup.ts'],
    include: ['src/**/*.test.{ts,tsx}'],
    css: false,
    pool: 'forks',
    poolOptions: {
      forks: {
        // Node >= 25 ships its own `localStorage` global, which shadows jsdom's and is undefined
        // without `--localstorage-file`. Turn it off so tests see the browser's Storage.
        execArgv: ['--no-experimental-webstorage'],
      },
    },
  },
  resolve: {
    alias: {
      '@': path.resolve(__dirname, 'src'),
    },
  },
  esbuild: { jsx: 'automatic' },
});
