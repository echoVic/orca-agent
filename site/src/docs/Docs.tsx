import {
  Suspense,
  useEffect,
  useMemo,
  useState,
  type ComponentType,
  type ReactNode,
} from "react";
import {
  applySeoHead,
  canonicalOrigin,
  detectInitialLocale,
  links,
  localeStorageKey,
  releaseVersion,
  type Locale,
  type SeoEntry,
} from "../shared";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkSlug from "remark-slug";
import rehypeHighlight from "rehype-highlight";
import { MDXProvider } from "@mdx-js/react";
import { mdxComponents } from "./mdx-components";
import "highlight.js/styles/github-dark.css";

const canonicalUrl = `${canonicalOrigin}/docs/`;

const seoCopy: Record<Locale, SeoEntry> = {
  en: {
    title: "Orca Docs — DeepSeek-native terminal coding agent documentation",
    description:
      "Complete Orca documentation: CLI reference, TUI commands, approval modes, goal mode, workflows, MCP integration, context management, and DeepSeek model configuration.",
    ogTitle: "Orca Docs — Complete reference for the DeepSeek-native coding agent",
    ogDescription:
      "Explore every Orca module: CLI flags, TUI sessions, approval modes, goal mode, workflows, MCP tools, context window, prefix cache, and the full configuration surface.",
    imageAlt: "Orca terminal coding agent documentation preview",
    locale: "en_US",
  },
  zh: {
    title: "Orca 文档 — DeepSeek 原生终端代码智能体参考",
    description:
      "完整的 Orca 使用文档：CLI 参考、TUI 命令、审批模式、持久目标、工作流、MCP 集成、上下文管理、DeepSeek 模型配置等。",
    ogTitle: "Orca 文档 — DeepSeek 原生代码智能体完整参考",
    ogDescription:
      "探索 Orca 各个模块：CLI 参数、TUI 会话、审批模式、Goal 模式、Workflow、MCP 工具、上下文窗口、前缀缓存及完整配置项。",
    imageAlt: "Orca 终端代码智能体文档预览",
    locale: "zh_CN",
  },
};

const navCopy: Record<
  Locale,
  {
    aria: { home: string; nav: string; language: string; langName: string };
    nav: { home: string; docs: string; changelog: string; install: string; github: string };
    brand: string;
  }
> = {
  en: {
    aria: { home: "Orca home", nav: "Main navigation", language: "Language", langName: "English" },
    nav: { home: "Home", docs: "Docs", changelog: "Changelog", install: "Install", github: "GitHub" },
    brand: "Orca",
  },
  zh: {
    aria: { home: "Orca 首页", nav: "主导航", language: "语言", langName: "中文" },
    nav: { home: "首页", docs: "文档", changelog: "更新日志", install: "安装", github: "GitHub" },
    brand: "Orca",
  },
};

type DocSection = { id: string; title: string; file: string };
type NavGroup = { label: string; items: DocSection[] };

const navEn: NavGroup[] = [
  {
    label: "Get Started",
    items: [
      { id: "overview", title: "Overview", file: "overview" },
      { id: "quickstart", title: "Quickstart", file: "quickstart" },
      { id: "cli-reference", title: "CLI Reference", file: "cli-reference" },
      { id: "configuration", title: "Configuration", file: "configuration" },
    ],
  },
  {
    label: "Core Concepts",
    items: [
      { id: "agent-loop", title: "Agent Loop", file: "agent-loop" },
      { id: "context-window", title: "Context Window", file: "context-window" },
      { id: "prefix-cache", title: "Prefix Cache", file: "prefix-cache" },
      { id: "models", title: "Models", file: "models" },
      { id: "approval-modes", title: "Approval Modes", file: "approval-modes" },
    ],
  },
  {
    label: "Interactive TUI",
    items: [
      { id: "session-commands", title: "Session Commands", file: "session-commands" },
      { id: "mention-search", title: "Mention Search", file: "mention-search" },
      { id: "side-conversations", title: "Side Conversations", file: "side-conversations" },
      { id: "goal-mode", title: "Goal Mode", file: "goal-mode" },
    ],
  },
  {
    label: "Headless & Automation",
    items: [
      { id: "orca-exec", title: "orca exec", file: "orca-exec" },
      { id: "orca-exec-resume", title: "orca exec resume", file: "orca-exec-resume" },
      { id: "verifier-gate", title: "Verifier Gate", file: "verifier-gate" },
      { id: "jsonl-output", title: "JSONL Output", file: "jsonl-output" },
      { id: "workflows", title: "Workflows", file: "workflows" },
    ],
  },
  {
    label: "Extend",
    items: [
      { id: "skills-plugins", title: "Skills & Plugins", file: "skills-plugins" },
      { id: "mcp-integration", title: "MCP Integration", file: "mcp-integration" },
    ],
  },
];

const navZh: NavGroup[] = [
  {
    label: "入门",
    items: [
      { id: "overview", title: "概述", file: "overview" },
      { id: "quickstart", title: "快速上手", file: "quickstart" },
      { id: "cli-reference", title: "CLI 参考", file: "cli-reference" },
      { id: "configuration", title: "配置说明", file: "configuration" },
    ],
  },
  {
    label: "核心概念",
    items: [
      { id: "agent-loop", title: "Agent 循环", file: "agent-loop" },
      { id: "context-window", title: "上下文窗口", file: "context-window" },
      { id: "prefix-cache", title: "前缀缓存", file: "prefix-cache" },
      { id: "models", title: "模型选择", file: "models" },
      { id: "approval-modes", title: "审批模式", file: "approval-modes" },
    ],
  },
  {
    label: "交互式 TUI",
    items: [
      { id: "session-commands", title: "会话命令", file: "session-commands" },
      { id: "mention-search", title: "Mention 搜索", file: "mention-search" },
      { id: "side-conversations", title: "侧边对话", file: "side-conversations" },
      { id: "goal-mode", title: "Goal 模式", file: "goal-mode" },
    ],
  },
  {
    label: "Headless & 自动化",
    items: [
      { id: "orca-exec", title: "orca exec", file: "orca-exec" },
      { id: "orca-exec-resume", title: "orca exec resume", file: "orca-exec-resume" },
      { id: "verifier-gate", title: "Verifier 门控", file: "verifier-gate" },
      { id: "jsonl-output", title: "JSONL 输出", file: "jsonl-output" },
      { id: "workflows", title: "Workflows 工作流", file: "workflows" },
    ],
  },
  {
    label: "扩展",
    items: [
      { id: "skills-plugins", title: "Skills 与插件", file: "skills-plugins" },
      { id: "mcp-integration", title: "MCP 集成", file: "mcp-integration" },
    ],
  },
];

const sectionsByLocale: Record<Locale, NavGroup[]> = { en: navEn, zh: navZh };
const defaultLocale: Locale = "en";

type MdxModule = { default: ComponentType };

const mdLoaders = (import.meta as any).glob("./md/**/*.md", {
  query: "?raw",
  import: "default",
  eager: false,
}) as Record<string, () => Promise<string>>;

const mdxLoaders = (import.meta as any).glob("./md/**/*.mdx", {
  eager: false,
}) as Record<string, () => Promise<MdxModule>>;

type LoadedContent =
  | { kind: "md"; text: string }
  | { kind: "mdx"; MdxComponent: ComponentType }
  | null;

const contentCache = new Map<string, Promise<LoadedContent>>();

function loadContent(locale: Locale, file: string): Promise<LoadedContent> {
  const langDir = locale === "zh" ? "zh" : "en";
  const key = `${langDir}:${file}`;
  if (contentCache.has(key)) return contentCache.get(key)!;

  const mdxKey = `./md/${langDir}/${file}.mdx`;
  const mdKey = `./md/${langDir}/${file}.md`;

  const promise = (async () => {
    if (mdxLoaders[mdxKey]) {
      const mod = await mdxLoaders[mdxKey]();
      return { kind: "mdx" as const, MdxComponent: mod.default };
    }
    if (mdLoaders[mdKey]) {
      const text = await mdLoaders[mdKey]();
      return { kind: "md" as const, text };
    }
    return null;
  })();

  contentCache.set(key, promise);
  return promise;
}

function MdxRenderer({ component }: { component: ComponentType }) {
  const Comp = component;
  return (
    <MDXProvider components={mdxComponents}>
      <Comp />
    </MDXProvider>
  );
}

function MarkdownRenderer({ text }: { text: string }) {
  return (
    <ReactMarkdown
      remarkPlugins={[remarkGfm, remarkSlug] as any}
      rehypePlugins={[rehypeHighlight] as any}
    >
      {text}
    </ReactMarkdown>
  );
}

function LoadingSkeleton() {
  return (
    <div className="docs-markdown">
      <div
        style={{
          padding: "2rem 0",
          color: "var(--faint)",
          fontSize: "0.875rem",
          textAlign: "center",
        }}
      >
        Loading…
      </div>
    </div>
  );
}

export default function Docs() {
  const [locale, setLocale] = useState<Locale>(() => detectInitialLocale() as Locale);
  const [activeId, setActiveId] = useState<string>("overview");
  const [loaded, setLoaded] = useState<LoadedContent>(null);
  const [loading, setLoading] = useState(false);

  const nav = sectionsByLocale[locale];

  const activeSection = useMemo(() => {
    const flat: Record<string, DocSection> = {};
    nav.forEach((g) => g.items.forEach((s) => (flat[s.id] = s)));
    return flat[activeId];
  }, [nav, activeId]);

  useEffect(() => {
    const copy = seoCopy[locale] || seoCopy[defaultLocale];
    applySeoHead(locale, copy, canonicalUrl);
  }, [locale]);

  useEffect(() => {
    try {
      localStorage.setItem(localeStorageKey, locale);
    } catch {}
  }, [locale]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    const raw = window.location.hash.replace(/^#/, "").trim();
    if (raw) setActiveId(raw.split("/")[0]);
  }, []);

  useEffect(() => {
    const onHash = () => {
      if (typeof window === "undefined") return;
      const raw = window.location.hash.replace(/^#/, "").trim();
      if (raw) setActiveId(raw.split("/")[0]);
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  useEffect(() => {
    if (!activeSection) {
      setLoaded(null);
      return;
    }
    let cancelled = false;
    setLoading(true);
    loadContent(locale, activeSection.file).then((content) => {
      if (!cancelled) {
        setLoaded(content);
        setLoading(false);
      }
    });
    return () => {
      cancelled = true;
    };
  }, [locale, activeSection]);

  const handleNavClick = (id: string) => {
    setActiveId(id);
    if (typeof window !== "undefined") {
      if (window.location.hash !== `#${id}`) {
        window.history.replaceState(null, "", `#${id}`);
      }
    }
  };

  const pageTitle = locale === "zh" ? "Orca 文档" : "Orca Documentation";
  const t = navCopy[locale];

  let content: ReactNode;
  if (loading && !loaded) {
    content = <LoadingSkeleton />;
  } else if (!loaded) {
    content = <p>Section not found.</p>;
  } else if (loaded.kind === "md") {
    content = <MarkdownRenderer text={loaded.text} />;
  } else {
    content = <MdxRenderer component={loaded.MdxComponent} />;
  }

  return (
    <div className="docs-page">
      <header className="nav">
        <a className="brand" href={links.home} aria-label={t.aria.home}>
          <img className="brand-mark" src="/orca-icon.svg" alt="" aria-hidden="true" />
          <span>{t.brand}</span>
        </a>
        <div className="nav-actions">
          <nav aria-label={t.aria.nav}>
            <a href={links.home}>{t.nav.home}</a>
            <a href={links.docs}>{t.nav.docs}</a>
            <a href={`${links.home}#install`}>{t.nav.install}</a>
            <a href={links.changelog}>{t.nav.changelog}</a>
            <a className="nav-cta" href={links.github} rel="noreferrer">
              {t.nav.github}
            </a>
          </nav>
          <div className="locale-switch" role="group" aria-label={t.aria.language}>
            <button
              type="button"
              aria-pressed={locale === "en"}
              aria-label={navCopy.en.aria.langName}
              onClick={() => setLocale("en")}
            >
              EN
            </button>
            <button
              type="button"
              aria-pressed={locale === "zh"}
              aria-label={navCopy.zh.aria.langName}
              onClick={() => setLocale("zh")}
            >
              中文
            </button>
          </div>
        </div>
      </header>

      <div className="docs-layout">
        <aside className="docs-nav">
          {nav.map((group) => (
            <div className="docs-nav-group" key={group.label}>
              <div className="docs-nav-group-title">{group.label}</div>
              <ul className="docs-nav-items">
                {group.items.map((item) => (
                  <li key={item.id}>
                    <a
                      href={`#${item.id}`}
                      className={
                        "docs-nav-link" + (activeId === item.id ? " is-active" : "")
                      }
                      onClick={(e) => {
                        e.preventDefault();
                        handleNavClick(item.id);
                      }}
                    >
                      {item.title}
                    </a>
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </aside>

        <main className="docs-main">
          <div className="docs-content">
            <h1 className="docs-page-title">
              {pageTitle} <span className="docs-version">{releaseVersion}</span>
            </h1>
            <div className="docs-markdown">
              <Suspense fallback={<LoadingSkeleton />}>{content}</Suspense>
            </div>
          </div>
        </main>
      </div>
    </div>
  );
}
