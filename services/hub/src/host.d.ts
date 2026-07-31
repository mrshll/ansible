/**
 * Globals the SpacetimeDB host provides that the package does not re-export.
 *
 * `spacetimedb/server` ships a typed `console` in `dist/server/console.d.ts` but
 * does not re-export it from its entry point, and the module runs with no DOM and
 * no Node globals — so `types: []` and `lib: es2023` leave it undeclared. Rather
 * than pull in `@types/node` (which would also declare `process`, `Buffer`, and a
 * filesystem that does not exist here) this declares exactly what the host offers.
 *
 * Log sparingly: module logs go to the database's own log, not to a terminal.
 */
declare const console: {
  log(...args: unknown[]): void;
  warn(...args: unknown[]): void;
  error(...args: unknown[]): void;
};
