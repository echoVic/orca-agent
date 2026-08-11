import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { resolve, dirname, basename, extname } from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const SITE_ROOT = resolve(__dirname, "..");

const HEADING_RE = /^(#{1,6})\s+(.+?)\s*#*\s*$/;
const FENCE_OPEN_RE = /^```(\S*)\s*$/;
const HR_RE = /^---+\s*$/;
const TABLE_SEP_RE = /^\s*\|?\s*:?-{2,}:?(\s*\|\s*:?-{2,}:?)+\s*\|?\s*$/;
const BQ_RE = /^>/m;
const UL_RE = /^(\s*)[-*+]\s+/;
const OL_RE = /^(\s*)\d+\.\s+/;

function tokenize(md) {
  const lines = md.replace(/\r\n/g, "\n").split("\n");
  const tokens = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) {
      tokens.push({ type: "empty", raw: line });
      i++;
      continue;
    }
    const h = line.match(HEADING_RE);
    if (h) {
      const level = h[1].length;
      tokens.push({
        type: "h" + level,
        level,
        text: h[2],
        raw: line,
      });
      i++;
      continue;
    }
    const fence = line.match(FENCE_OPEN_RE);
    if (fence) {
      const lang = fence[1] || "";
      const start = i;
      i++;
      while (i < lines.length && !/^```\s*$/.test(lines[i])) i++;
      const end = i;
      i++;
      const code = lines.slice(start + 1, end).join("\n");
      const raw = lines.slice(start, end + 1).join("\n");
      tokens.push({ type: "fence", lang, code, raw });
      continue;
    }
    if (HR_RE.test(line)) {
      tokens.push({ type: "hr", raw: line });
      i++;
      continue;
    }
    if (TABLE_SEP_RE.test(line) && i > 0) {
      let tStart = i - 1;
      while (tStart > 0 && lines[tStart - 1].trim() !== "") tStart--;
      let tEnd = i + 1;
      while (tEnd < lines.length && lines[tEnd].includes("|") && lines[tEnd].trim() !== "") tEnd++;
      const raw = lines.slice(tStart, tEnd).join("\n");
      for (let k = tokens.length - 1; k >= 0; k--) {
        if (tokens[k].type === "empty") tokens.pop();
        else break;
      }
      tokens.push({ type: "table", raw });
      i = tEnd;
      continue;
    }
    if (BQ_RE.test(line)) {
      const start = i;
      while (i < lines.length && (BQ_RE.test(lines[i]) || lines[i].trim() === "")) i++;
      tokens.push({ type: "blockq", raw: lines.slice(start, i).join("\n") });
      continue;
    }
    if (UL_RE.test(line)) {
      const start = i;
      while (i < lines.length && (UL_RE.test(lines[i]) || OL_RE.test(lines[i]) || (lines[i].trim() !== "" && /^\s{2,}/.test(lines[i])))) i++;
      tokens.push({ type: "ul", raw: lines.slice(start, i).join("\n") });
      continue;
    }
    if (OL_RE.test(line)) {
      const start = i;
      const items = [];
      while (i < lines.length) {
        if (OL_RE.test(lines[i])) items.push(lines[i]);
        else if (lines[i].trim() === "") {
          if (i + 1 < lines.length && OL_RE.test(lines[i + 1])) i++;
          else break;
        } else if (/^\s{2,}/.test(lines[i]) || UL_RE.test(lines[i])) {
          i++;
          continue;
        } else break;
        i++;
      }
      tokens.push({ type: "ol", raw: lines.slice(start, i).join("\n"), items: items.length });
      continue;
    }
    const start = i;
    while (
      i < lines.length &&
      lines[i].trim() !== "" &&
      !HEADING_RE.test(lines[i]) &&
      !FENCE_OPEN_RE.test(lines[i]) &&
      !HR_RE.test(lines[i])
    )
      i++;
    tokens.push({ type: "para", text: lines.slice(start, i).join(" "), raw: lines.slice(start, i).join("\n") });
  }
  return tokens;
}

const CALLOUT_PATTERNS = [
  { re: /^Pro tip[:：]\s*/i, type: "info", title: () => "Pro tip" },
  { re: /^Tip[:：]\s*/i, type: "info", title: () => "Tip" },
  { re: /^Note[:：]\s*/i, type: "info", title: () => "Note" },
  { re: /^Important[:：]\s*/i, type: "warning", title: () => "Important" },
  { re: /^Notice[:：]\s*/i, type: "info", title: () => "Notice" },
  { re: /^Warning[:：]\s*/i, type: "warning", title: () => "Warning" },
  { re: /^Caution[:：]\s*/i, type: "warning", title: () => "Caution" },
  { re: /^Security[:：]\s*/i, type: "danger", title: () => "Security" },
  { re: /^Danger[:：]\s*/i, type: "danger", title: () => "Danger" },
  { re: /^Best practice[:：]\s*/i, type: "success", title: () => "Best practice" },
  { re: /^提示[:：]\s*/, type: "info", title: () => "提示" },
  { re: /^小贴士[:：]\s*/, type: "info", title: () => "小贴士" },
  { re: /^注意[:：]\s*/, type: "warning", title: () => "注意" },
  { re: /^警告[:：]\s*/, type: "warning", title: () => "警告" },
  { re: /^危险[:：]\s*/, type: "danger", title: () => "危险" },
  { re: /^安全[:：]\s*/, type: "danger", title: () => "安全提示" },
  { re: /^最佳实践[:：]\s*/, type: "success", title: () => "最佳实践" },
  { re: /^推荐[:：]\s*/, type: "success", title: () => "推荐" },
];

const CALLOUT_HINT_RE = [
  /recommended/i,
  /for production/i,
  /生产环境/i,
  /never /i,
  /always /i,
  /do not /i,
  /don['’]t /i,
  /不可/,
  /切勿/,
  /务必/,
  /建议/,
];

function detectCallout(para, _lang) {
  for (const pat of CALLOUT_PATTERNS) {
    const m = para.match(pat.re);
    if (m) {
      const title = pat.title ? pat.title(m) : undefined;
      const body = para.replace(pat.re, "").trim();
      return { type: pat.type, title, body };
    }
  }
  const hasHint = CALLOUT_HINT_RE.some((r) => r.test(para));
  if (hasHint) {
    const type = /never|do not|don['’]t|不可|切勿|danger|security/i.test(para)
      ? "danger"
      : /recommended|推荐|production|生产环境|建议/i.test(para)
        ? "info"
        : "warning";
    return { type, body: para };
  }
  return null;
}

const TAB_LABEL_RE = [
  /^Option\s+[A-Z0-9][^a-z]*$/i,
  /^选项\s*[A-Z0-9一二三四五六七八九十]+[:：]?\s*[^，。]{0,10}$/,
  /^方法\s*[一二三四五六七八九十\d]+[:：]?\s*[^，。]{0,10}$/,
  /^方式\s*[一二三四五六七八九十\d]+[:：]?\s*[^，。]{0,10}$/,
  /^环境[:：]?\s*[\w\s/()\-. ]{0,30}$/,
  /^Platform[:：]?\s*[\w\s/()\-. ]{0,30}$/i,
  /^macOS$/i,
  /^Linux$/i,
  /^Windows$/i,
  /^npm\s*\(.+\)$/i,
  /^PowerShell$/i,
  /^DeepSeek-V4-Flash.*$/i,
  /^DeepSeek-V4-Pro.*$/i,
  /^deepseek-v4-(flash|pro).*$/i,
  /^\(?(推荐|Recommended|Default|默认)?\)?\s*npm\s*\(.+\)\s*$/i,
  /^原生安装器/i,
  /^Windows PowerShell$/i,
  /^环境变量(\s*\(.+\))?\s*$/i,
  /^Environment variable[s]?(\s*\(.+\))?\s*$/i,
  /^配置文件\s*$/i,
  /^Config file\s*$/i,
];

function looksLikeTabTitle(headingText) {
  return TAB_LABEL_RE.some((r) => r.test(headingText.trim()));
}

const TAB_CLEANUP = [
  /^Option\s+[A-Z0-9][^a-z]*[:：]?\s*/i,
  /^选项\s*[A-Z0-9一二三四五六七八九十]+[:：]?\s*/,
  /^方法\s*[一二三四五六七八九十\d]+[:：]?\s*/,
  /^方式\s*[一二三四五六七八九十\d]+[:：]?\s*/,
  /^环境[:：]?\s*/,
  /^Platform[:：]?\s*/i,
  /^\((?:推荐|Recommended|Default|默认)\)\s*/i,
  /\s*\((?:recommended|推荐)\)$/i,
  /\s*\((?:默认|Default)\)$/i,
];

function headingToTabLabel(text) {
  const t = text.trim();
  let out = t;
  for (const r of TAB_CLEANUP) out = out.replace(r, "");
  return out || t;
}

function groupTabs(tokens) {
  const out = [];
  let i = 0;
  while (i < tokens.length) {
    const tok = tokens[i];
    if ((tok.type === "h3" || tok.type === "h4") && looksLikeTabTitle(tok.text)) {
      const level = tok.level;
      const tabHeadings = [];
      let j = i;
      while (j < tokens.length) {
        const t = tokens[j];
        if ("level" in t) {
          if (t.level === level) {
            if (looksLikeTabTitle(t.text)) tabHeadings.push(j);
            else break;
          } else if (t.level < level) {
            break;
          }
        }
        j++;
      }
      if (tabHeadings.length >= 2) {
        const chunks = [];
        for (let k = 0; k < tabHeadings.length; k++) {
          const s = tabHeadings[k];
          const e = k + 1 < tabHeadings.length ? tabHeadings[k + 1] : Math.min(j, tokens.length);
          const headingTok = tokens[s];
          chunks.push({
            label: headingToTabLabel(headingTok.text),
            body: tokens.slice(s + 1, e),
          });
        }
        out.push({ type: "__tabs", chunks });
        i = Math.min(j, tokens.length);
        continue;
      }
    }
    out.push(tok);
    i++;
  }
  return out;
}

function tokenToMdx(tok, lang) {
  if (tok.type === "__tabs") {
    const { chunks } = tok;
    let s = "\n<Tabs>\n";
    for (const chunk of chunks) {
      s += `<Tab label={${JSON.stringify(chunk.label)}}>\n\n`;
      for (const child of chunk.body) s += tokenToMdx(child, lang) + "\n";
      s += "\n</Tab>\n";
    }
    s += "</Tabs>\n";
    return s;
  }
  if (tok.type === "para") {
    const cal = detectCallout(tok.text, lang);
    if (cal) {
      const titleAttr = cal.title ? ` title={${JSON.stringify(cal.title)}}` : "";
      return `<Callout type="${cal.type}"${titleAttr}>\n\n${cal.body}\n\n</Callout>`;
    }
    return tok.raw;
  }
  if (tok.type === "ol" && tok.items >= 3 && tok.items <= 8) {
    const lines = tok.raw.split("\n");
    const steps = [];
    let cur = null;
    for (const line of lines) {
      const m = line.match(/^\s*\d+\.\s*(.+)$/);
      if (m) {
        const head = m[1];
        let title = "";
        let rest = head;
        const split = head.match(/^(.+?)[.:：]\s*(.+)$/);
        if (split && split[1].length <= 40) {
          title = split[1];
          rest = split[2];
        }
        cur = { title: title || undefined, body: [] };
        if (rest) cur.body.push(rest);
        steps.push(cur);
      } else if (cur && line.trim()) {
        cur.body.push(line.replace(/^\s{2,}/, ""));
      }
    }
    if (steps.length === tok.items && steps.every((s) => s.body.length > 0)) {
      let s = "\n<Steps>\n";
      for (const step of steps) {
        s += step.title ? `<Step title={${JSON.stringify(step.title)}}>\n\n` : "<Step>\n\n";
        s += step.body.join("\n") + "\n\n</Step>\n";
      }
      s += "</Steps>\n";
      return s;
    }
  }
  return tok.raw;
}

const MDX_IMPORT = `import { Callout, Steps, Step, Tabs, Tab } from "../../mdx-components";

`;

function convert(md, _lang) {
  const tokens = tokenize(md);
  const transformed = groupTabs(tokens);
  let out = MDX_IMPORT;
  let idx = 0;
  while (idx < transformed.length && transformed[idx].type === "empty") idx++;
  if (transformed[idx]?.type === "h1") {
    out += transformed[idx].raw + "\n\n";
    idx++;
  }
  for (let k = idx; k < transformed.length; k++) {
    const tok = transformed[k];
    const rendered = tokenToMdx(tok, "en");
    out += rendered + "\n";
  }
  out = out.replace(/\n{4,}/g, "\n\n\n");
  return out.trimEnd() + "\n";
}

function processDir(dir, lang) {
  const files = readdirSync(dir).filter((f) => extname(f) === ".md");
  let ok = 0;
  for (const f of files) {
    const mdPath = resolve(dir, f);
    const mdxPath = mdPath.replace(/\.md$/, ".mdx");
    const md = readFileSync(mdPath, "utf8");
    const mdx = convert(md, lang);
    writeFileSync(mdxPath, mdx);
    ok++;
  }
  console.log(`    converted ${ok}/${files.length} files in ${dir}`);
  return ok;
}

const enDir = resolve(SITE_ROOT, "src/docs/md/en");
const zhDir = resolve(SITE_ROOT, "src/docs/md/zh");
console.log("Converting EN docs...");
processDir(enDir, "en");
console.log("Converting ZH docs...");
processDir(zhDir, "zh");
console.log("Done.");
