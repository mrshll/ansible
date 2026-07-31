/**
 * The module entry point.
 *
 * SpacetimeDB names a reducer after the export it is bound to, so this file is
 * also the wire surface: re-exporting `reducers.js` is what makes
 * `update_session_status` callable, and re-exporting `rls.js` is what registers
 * the visibility filters. An export removed from here is a reducer that no longer
 * exists.
 *
 *   spacetime publish --project-path services/hub ansible-hub
 */

export { spacetimedb as default } from "./schema.js";
export * from "./reducers.js";
export * from "./rls.js";
