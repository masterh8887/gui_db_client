// Re-exports so components can `import { useJobsStore } from '@/stores'`. Nothing inside
// `stores/` or `lib/` imports this barrel (DESIGN.md §7.1): they import the module directly.
export * from './catalog';
export * from './connections';
export * from './extract';
export * from './history';
export * from './jobs';
export * from './logs';
export * from './settings';
export * from './tabs';
export * from './ui';
export * from './workspace';
