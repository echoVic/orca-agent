import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import mdx from "@mdx-js/rollup";
import remarkGfm from "remark-gfm";
import remarkSlug from "remark-slug";
import rehypeHighlight from "rehype-highlight";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL(".", import.meta.url));

export default defineConfig({
  plugins: [
    {
      enforce: "pre",
      ...mdx({
        providerImportSource: "@mdx-js/react",
        remarkPlugins: [remarkGfm, remarkSlug],
        rehypePlugins: [rehypeHighlight],
      }),
    },
    react(),
  ],
  base: "/",
  build: {
    rollupOptions: {
      input: {
        main: resolve(root, "index.html"),
        changelog: resolve(root, "changelog/index.html"),
        terminalCodingAgent: resolve(root, "terminal-coding-agent/index.html"),
        deepseekCodingAgent: resolve(root, "deepseek-coding-agent/index.html"),
        githubWorkflows: resolve(root, "github/index.html"),
        mcp: resolve(root, "mcp/index.html"),
        docs: resolve(root, "docs/index.html"),
      },
    },
  },
});
