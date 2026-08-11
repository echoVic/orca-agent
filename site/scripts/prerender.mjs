import { readFile, writeFile } from "node:fs/promises";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";

// Inject server-rendered HTML into each built page so search-engine and AI
// crawlers (which often don't execute JS) see real content instead of an empty
// <div id="root">. Runs after `vite build`, against the files in dist/.

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const mountPoint = '<div id="root"></div>';

const routes = [
  { route: "/", file: "dist/index.html" },
  { route: "/changelog/", file: "dist/changelog/index.html" },
  { route: "/docs/", file: "dist/docs/index.html" },
];

const vite = await createServer({
  root,
  appType: "custom",
  server: { middlewareMode: true, hmr: false, ws: false },
  logLevel: "warn",
});

try {
  const { render } = await vite.ssrLoadModule("/src/entry-server.tsx");

  for (const { route, file } of routes) {
    const path = resolve(root, file);
    const template = await readFile(path, "utf8");

    if (!template.includes(mountPoint)) {
      throw new Error(`Mount point ${mountPoint} not found in ${file}`);
    }

    let appHtml;
    try {
      appHtml = render(route);
    } catch (err) {
      if (route === "/docs/" && err instanceof RangeError) {
        // Docs page has a large JSX tree; skip SSR and rely on client-side
        // hydration. The page will still render fully in the browser.
        console.log(`skipped prerender for ${route} (SSR stack overflow, client-only)`);
        continue;
      }
      throw err;
    }
    if (!appHtml.trim()) {
      throw new Error(`Render for ${route} produced empty HTML`);
    }

    const output = template.replace(mountPoint, `<div id="root">${appHtml}</div>`);
    await writeFile(path, output);
    console.log(`prerendered ${route} -> ${file} (${appHtml.length} chars)`);
  }
} finally {
  await vite.close();
}
