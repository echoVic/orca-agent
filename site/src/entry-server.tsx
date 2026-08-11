import { StrictMode } from "react";
import { renderToString } from "react-dom/server";
import App from "./App";
import Changelog from "./changelog/Changelog";
import Docs from "./docs/Docs";

/**
 * Server entry used only at build time by scripts/prerender.mjs.
 *
 * Returns the static HTML for a route so the crawler-visible markup is no
 * longer an empty <div id="root">. Components are SSR-safe: locale detection
 * returns "en" when `window` is undefined and every browser API call lives in
 * a useEffect/handler, so renderToString never touches the DOM.
 */
export function render(route: string): string {
  let Component;
  if (route === "/changelog/") {
    Component = Changelog;
  } else if (route === "/docs/") {
    Component = Docs;
  } else {
    Component = App;
  }
  return renderToString(
    <StrictMode>
      <Component />
    </StrictMode>,
  );
}
