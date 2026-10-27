import { existsSync, readFileSync } from "node:fs";

const root = new URL("..", import.meta.url);
const canonicalUrl = "https://orcaagent.dev/";
const changelogUrl = "https://orcaagent.dev/changelog/";
const intentPages = [
  {
    path: "terminal-coding-agent/",
    url: "https://orcaagent.dev/terminal-coding-agent/",
    title: "Terminal coding agent for DeepSeek",
    terms: ["orca terminal", "terminal coding agent", "orca agent"],
  },
  {
    path: "deepseek-coding-agent/",
    url: "https://orcaagent.dev/deepseek-coding-agent/",
    title: "DeepSeek coding agent",
    terms: ["deepseek orca", "DeepSeek coding agent", "DeepSeek-native"],
  },
  {
    path: "github/",
    url: "https://orcaagent.dev/github/",
    title: "Orca GitHub coding workflows",
    terms: ["orca github", "GitHub workflows", "verifier-gated"],
  },
  {
    path: "mcp/",
    url: "https://orcaagent.dev/mcp/",
    title: "Orca MCP tools",
    terms: ["orca mcp", "MCP tools", "spec-driven tools"],
  },
];
const indexHtml = readFileSync(new URL("index.html", root), "utf8");
const changelogHtml = readFileSync(new URL("changelog/index.html", root), "utf8");
const sharedSource = readFileSync(new URL("src/shared.ts", root), "utf8");
const appSource = readFileSync(new URL("src/App.tsx", root), "utf8");
const changelogSource = readFileSync(new URL("src/changelog/Changelog.tsx", root), "utf8");
const styles = readFileSync(new URL("src/styles.css", root), "utf8");
const readme = readFileSync(new URL("../README.md", root), "utf8");
const robotsTxt = readFileSync(new URL("public/robots.txt", root), "utf8");
const sitemapXml = readFileSync(new URL("public/sitemap.xml", root), "utf8");
const socialPng = readFileSync(new URL("public/orca-social.png", root));
const npmPackage = JSON.parse(readFileSync(new URL("../npm/orca/package.json", root), "utf8"));

const failures = [];

function check(condition, message) {
  if (!condition) {
    failures.push(message);
  }
}

function includes(value, message) {
  check(indexHtml.includes(value), message);
}

const latestRelease = sharedSource.match(
  /releaseVersion\s*=\s*"v([^"]+)"[\s\S]*?version:\s*"v\1"[\s\S]*?date:\s*"([^"]+)"/,
);
check(latestRelease, "Could not parse latest release version/date from shared.ts");
const latestVersion = latestRelease?.[1] ?? "";
const latestDate = latestRelease?.[2] ?? "";

includes(`<link rel="canonical" href="${canonicalUrl}" />`, "Missing canonical URL");
includes('<meta name="robots" content="index, follow" />', "Missing robots index directive");
includes('<meta property="og:type" content="website" />', "Missing Open Graph type");
includes(`<meta property="og:url" content="${canonicalUrl}" />`, "Missing Open Graph URL");
includes(
  '<meta property="og:image" content="https://orcaagent.dev/orca-social.png" />',
  "Missing Open Graph PNG image",
);
includes('<meta property="og:image:width" content="1200" />', "Missing Open Graph image width");
includes('<meta property="og:image:height" content="630" />', "Missing Open Graph image height");
includes('<meta name="twitter:card" content="summary_large_image" />', "Missing Twitter card");
includes(
  '<meta name="twitter:image" content="https://orcaagent.dev/orca-social.png" />',
  "Missing Twitter PNG image",
);
includes('<script type="application/ld+json">', "Missing JSON-LD block");
includes('"@type": "SoftwareApplication"', "Missing SoftwareApplication schema");
includes('"@type": "WebSite"', "Missing WebSite schema");
includes('"@type": "Organization"', "Missing Organization schema");

check(robotsTxt.includes("User-agent: *"), "robots.txt missing user agent rule");
check(robotsTxt.includes("Allow: /"), "robots.txt missing allow rule");
check(robotsTxt.includes(`Sitemap: ${canonicalUrl}sitemap.xml`), "robots.txt missing sitemap URL");

check(sitemapXml.includes("<urlset"), "sitemap.xml missing urlset");
check(sitemapXml.includes(`<loc>${canonicalUrl}</loc>`), "sitemap.xml missing canonical loc");
check(sitemapXml.includes(`<loc>${changelogUrl}</loc>`), "sitemap.xml missing changelog loc");
for (const page of intentPages) {
  check(sitemapXml.includes(`<loc>${page.url}</loc>`), `sitemap.xml missing ${page.url}`);
}
const docsUrl = "https://orcaagent.dev/docs/";
check(sitemapXml.includes(`<loc>${docsUrl}</loc>`), "sitemap.xml missing docs loc");
check(
  sitemapXml.includes(`<lastmod>${latestDate}</lastmod>`),
  "sitemap.xml lastmod must match the latest release date",
);
check(
  npmPackage.version === latestVersion,
  "npm package version must match the site releaseVersion",
);

// Homepage must expose FAQPage structured data for the FAQ section.
// The FAQ JSON-LD is rendered by the App component and injected during
// prerender, so check the built output rather than the source template.
const distIndexPath = new URL("dist/index.html", root);
if (existsSync(distIndexPath)) {
  const distIndexHtml = readFileSync(distIndexPath, "utf8");
  check(
    /"@type":\s*"FAQPage"/.test(distIndexHtml),
    "Prerendered homepage missing FAQPage JSON-LD structured data",
  );
} else {
  check(
    appSource.includes('"@type": "FAQPage"'),
    "App source must render FAQPage JSON-LD structured data",
  );
}
// Organization sameAs should link the author site so entity clustering works.
check(
  indexHtml.includes("https://echovic.com"),
  "Homepage Organization sameAs missing echovic.com",
);
// Favicon must resolve so /favicon.ico is not a 404.
check(
  existsSync(new URL("public/favicon.ico", root)),
  "Missing public/favicon.ico (causes /favicon.ico 404)",
);

check(
  changelogHtml.includes(`<link rel="canonical" href="${changelogUrl}" />`),
  "changelog page missing canonical URL",
);
check(
  changelogHtml.includes(`<meta property="og:url" content="${changelogUrl}" />`),
  "changelog page missing og:url",
);
check(
  changelogHtml.includes('<title>Orca changelog</title>'),
  "changelog page missing title",
);
check(
  /releases\.map/.test(changelogSource),
  "Changelog component must render releases list",
);
check(
  changelogSource.includes("Terminal coding agent") &&
    changelogSource.includes("DeepSeek coding agent") &&
    changelogSource.includes("GitHub workflows"),
  "Changelog React source must own search-intent guide links",
);
check(
  !changelogHtml.includes("crawler-links"),
  "Changelog template must not render root-level crawler links",
);
check(
  changelogHtml.includes('"@type": "CollectionPage"'),
  "changelog page missing CollectionPage JSON-LD structured data",
);

check(socialPng.subarray(1, 4).toString("ascii") === "PNG", "Social image is not a PNG");
check(socialPng.readUInt32BE(16) === 1200, "Social PNG width must be 1200px");
check(socialPng.readUInt32BE(20) === 630, "Social PNG height must be 630px");

check(appSource.includes("472309526"), "Homepage missing official QQ group");
check(
  sharedSource.includes("https://t.me/+11No1w5ZbTMyZTQ1"),
  "Site shared.ts missing official Telegram group link",
);
check(readme.includes("472309526"), "README missing official QQ group");
check(
  readme.includes("https://t.me/+11No1w5ZbTMyZTQ1"),
  "README missing official Telegram group",
);
check(
  /\.hero-copy\s*\{[^}]*align-self:\s*start;/s.test(styles),
  "Hero copy must stay top-aligned while the terminal animation grows",
);
check(
  /@media\s*\(max-width:\s*860px\)[\s\S]*\.nav-actions nav a:not\(\.nav-cta\)\s*\{[\s\S]*display:\s*none;/s.test(
    styles,
  ),
  "Mobile nav must keep the GitHub CTA visible while hiding secondary links",
);

const requiredHomepageSource = [
  ["Quick Start", "Homepage missing English quick start section"],
  ["Common dev tasks", "Homepage missing English use-case section"],
  ["Why teams pick Orca", "Homepage missing English comparison section"],
  ["Frequently asked questions", "Homepage missing English FAQ section"],
  ["快速上手", "Homepage missing Chinese quick start section"],
  ["常见开发任务", "Homepage missing Chinese use-case section"],
  ["为什么选择 Orca", "Homepage missing Chinese comparison section"],
  ["常见问题", "Homepage missing Chinese FAQ section"],
  ['code: ["orca"]', "Quick start must launch the interactive TUI with orca"],
  ["guides you through the DeepSeek API key", "Quick start must describe guided key setup"],
  ["进入交互式终端", "Chinese quick start must describe entering the interactive terminal"],
  ["terminal coding agent", "Homepage missing terminal coding agent phrase"],
];

for (const [needle, message] of requiredHomepageSource) {
  check(appSource.includes(needle), message);
}

const forbiddenHomepageSource = [
  ["searchPathsTitle", "Homepage must not own a search-intent module"],
  ["search-paths", "Homepage must not render the search-intent section"],
  ["Explore orca terminal", "Homepage must not expose SEO query cards"],
];

for (const [needle, message] of forbiddenHomepageSource) {
  check(!appSource.includes(needle), message);
}

const viteConfig = readFileSync(new URL("vite.config.ts", root), "utf8");
for (const page of intentPages) {
  const pageUrl = new URL(page.path, root);
  const htmlPath = new URL("index.html", pageUrl);
  check(existsSync(htmlPath), `Missing landing page at ${page.path}index.html`);
  check(
    viteConfig.includes(`${page.path}index.html`),
    `Vite build input missing ${page.path}index.html`,
  );

  if (existsSync(htmlPath)) {
    const html = readFileSync(htmlPath, "utf8");
    check(
      html.includes(`<link rel="canonical" href="${page.url}" />`),
      `${page.path} missing canonical URL`,
    );
    check(html.includes(`<title>${page.title}</title>`), `${page.path} missing title`);
    check(html.includes('<meta name="robots" content="index, follow" />'), `${page.path} noindex`);
    check(html.includes('"@type": "TechArticle"'), `${page.path} missing TechArticle schema`);
    check(
      /"@type":\s*"FAQPage"/.test(html),
      `${page.path} missing FAQPage JSON-LD structured data`,
    );
    for (const term of page.terms) {
      check(html.includes(term), `${page.path} missing search term: ${term}`);
    }
    // Landing pages need substantive content to avoid thin-content penalties.
    const words = html.replace(/<[^>]*>/g, " ").split(/\s+/).filter(Boolean).length;
    check(words >= 1500, `${page.path} thin content: ${words} words (need >= 1500)`);
  }
}

// Custom 404 page must exist so unknown routes do not serve the host's default.
check(
  existsSync(new URL("public/404.html", root)),
  "Missing public/404.html (unknown routes serve a default 404)",
);

const jsonLdBlocks = [
  ...indexHtml.matchAll(
    /<script type="application\/ld\+json">([\s\S]*?)<\/script>/g,
  ),
].map((match) => match[1].trim());

for (const [index, block] of jsonLdBlocks.entries()) {
  try {
    JSON.parse(block);
  } catch (error) {
    failures.push(`JSON-LD block ${index + 1} is invalid: ${error.message}`);
  }
}

check(jsonLdBlocks.length >= 1, "No parseable JSON-LD blocks found");

if (failures.length > 0) {
  console.error("SEO check failed:");
  for (const failure of failures) {
    console.error(`- ${failure}`);
  }
  process.exit(1);
}

console.log("SEO check passed.");
