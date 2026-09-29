// Generates the /docs pages from the repo's own markdown, so the README
// and docs/ stay the single source of truth. Runs before `astro dev` and
// `astro build`; the output directory is gitignored.
//
// A page is either a whole file (its H1 dropped, since Starlight renders
// the title) or one README section (the heading becomes the title and
// its subheadings are promoted a level). Links are rewritten: targets
// that are themselves synced become site routes, README anchors map to
// the page that now holds that heading, and everything else points at
// the file on GitHub.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import GithubSlugger from "github-slugger";

const SITE = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const REPO = path.resolve(SITE, "..");
const OUT = path.join(SITE, "src/content/docs/docs");
const GH = "https://github.com/oximg/oximg";

/** @typedef {{ file: string, section?: string, level?: number, stopAt?: string, intro?: boolean }} Source */
/** @type {{ slug: string, title: string, description: string, sources: Source[] }[]} */
const PAGES = [
  {
    slug: "",
    title: "Overview",
    description: "What oximg is, what it does, and how it is built.",
    sources: [
      { file: "README.md", intro: true },
      { file: "README.md", section: "Features", level: 2, stopAt: "Supported formats" },
    ],
  },
  {
    slug: "install",
    title: "Install",
    description: "Docker, prebuilt binaries, Homebrew, Cargo, or from source.",
    sources: [{ file: "README.md", section: "Install", level: 2 }],
  },
  {
    slug: "serving",
    title: "Serving",
    description: "URL grammars, sources, signing, CORS, error classes and shutdown.",
    sources: [{ file: "README.md", section: "Serving", level: 2 }],
  },
  {
    slug: "formats",
    title: "Formats",
    description: "Decode and encode matrix, cross-format output, negotiation, orientation and ICC.",
    sources: [{ file: "README.md", section: "Supported formats", level: 3 }],
  },
  {
    slug: "pipeline",
    title: "Pipeline",
    description: "What happens between source bytes and the response.",
    sources: [{ file: "README.md", section: "Pipeline", level: 2 }],
  },
  {
    slug: "cli",
    title: "CLI",
    description: "One-shot commands over the same pipeline, no server.",
    sources: [{ file: "README.md", section: "CLI", level: 2 }],
  },
  {
    slug: "library",
    title: "Rust library",
    description: "The oximg::pipeline API: process, probe, typed errors, per-call overrides.",
    sources: [{ file: "README.md", section: "Library", level: 2 }],
  },
  {
    slug: "configuration",
    title: "Configuration",
    description: "Every environment variable, read once at startup, fail-closed.",
    sources: [{ file: "README.md", section: "Configuration", level: 2 }],
  },
  {
    slug: "errors",
    title: "Errors",
    description: "HTTP statuses and library ErrorKind, classified by fault.",
    sources: [{ file: "docs/features/errors.md" }],
  },
  {
    slug: "deploy/docker",
    title: "Docker",
    description: "Tag pinning, read-only mounts, remote origins and graceful stop.",
    sources: [{ file: "docs/deploy-docker.md" }],
  },
  {
    slug: "deploy/kubernetes",
    title: "Kubernetes",
    description: "An example Deployment with probes, limits, drain and URI-hash coalescing.",
    sources: [{ file: "docs/deploy-kubernetes.md" }],
  },
  {
    slug: "deploy/cloud-run",
    title: "Cloud Run",
    description: "The PORT contract, gs:// with the service identity, and concurrency sizing.",
    sources: [{ file: "docs/deploy-cloud-run.md" }],
  },
  {
    slug: "ruby/oximg",
    title: "oximg gem",
    description: "Local image processing from Ruby, no libvips or ImageMagick.",
    sources: [{ file: "rubygem/oximg/README.md" }],
  },
  {
    slug: "ruby/rails",
    title: "oximg-rails",
    description: "Signed server URLs and ActiveStorage integration for Rails.",
    sources: [{ file: "rubygem/oximg-rails/README.md" }],
  },
];

// README headings that no page holds, and where a link to them goes.
const README_ANCHOR_FALLBACK = {
  benchmarks: "/benchmarks/",
  deployment: "/docs/deploy/docker/",
};

const route = (slug) => (slug ? `/docs/${slug}/` : "/docs/");

// --- Markdown helpers ---------------------------------------------------

/** Parses headings, skipping fenced code (a `# comment` in a sh block is not a heading). */
function parse(file) {
  const lines = fs.readFileSync(path.join(REPO, file), "utf8").split("\n");
  const headings = [];
  let fence = null;
  lines.forEach((line, i) => {
    const f = line.match(/^\s*(```+|~~~+)/);
    if (f) {
      if (!fence) fence = f[1];
      else if (line.trim().startsWith(fence)) fence = null;
      return;
    }
    if (fence) return;
    const h = line.match(/^(#{1,6})\s+(.*?)\s*#*\s*$/);
    if (h) headings.push({ index: i, level: h[1].length, text: h[2] });
  });
  return { lines, headings };
}

/** Lines of one section, its subheadings promoted so they start at `##`. */
function extractSection(doc, { section, level, stopAt }) {
  const at = doc.headings.findIndex((h) => h.level === level && h.text === section);
  if (at < 0) throw new Error(`sync-docs: heading "${"#".repeat(level)} ${section}" not found`);
  const start = doc.headings[at];
  const after = doc.headings.slice(at + 1);
  const end =
    after.find((h) => h.level <= level || (stopAt && h.text === stopAt))?.index ?? doc.lines.length;
  const shift = level - 1;
  const inside = new Set(after.filter((h) => h.index < end).map((h) => h.index));
  return doc.lines.slice(start.index + 1, end).map((line, i) => {
    const abs = start.index + 1 + i;
    return inside.has(abs) ? line.replace(/^#+/, (m) => "#".repeat(m.length - shift)) : line;
  });
}

/** README text between the H1 and the first `##`, without the badge row. */
function extractIntro(doc) {
  const h1 = doc.headings.find((h) => h.level === 1);
  const next = doc.headings.find((h) => h.index > h1.index)?.index ?? doc.lines.length;
  return doc.lines.slice(h1.index + 1, next).filter((l) => !l.startsWith("[!["));
}

/** Whole file, H1 removed. */
function extractFile(doc) {
  const h1 = doc.headings.find((h) => h.level === 1);
  return h1 ? doc.lines.filter((_, i) => i !== h1.index) : doc.lines;
}

// --- Link resolution ------------------------------------------------------

const wholeFileRoutes = new Map(); // repo path -> route
for (const p of PAGES) {
  for (const s of p.sources) {
    if (!s.section && !s.intro) wholeFileRoutes.set(s.file, route(p.slug));
  }
}

// Every README heading -> the page (and anchor) that now holds it.
const readmeAnchors = new Map();
{
  const readme = parse("README.md");
  const slugger = new GithubSlugger();
  const owners = PAGES.flatMap((p) =>
    p.sources
      .filter((s) => s.file === "README.md" && s.section)
      .map((s) => ({ page: p, source: s })),
  );
  for (const h of readme.headings) {
    const anchor = slugger.slug(h.text);
    // The innermost section containing this heading owns it.
    let owner = null;
    for (const o of owners) {
      const start = readme.headings.find((x) => x.level === o.source.level && x.text === o.source.section);
      const after = readme.headings.filter((x) => x.index > start.index);
      const end =
        after.find((x) => x.level <= o.source.level || (o.source.stopAt && x.text === o.source.stopAt))
          ?.index ?? Infinity;
      if (h.index >= start.index && h.index < end) {
        if (!owner || start.index > owner.start) owner = { page: o.page, start: start.index, isTitle: h.index === start.index };
      }
    }
    if (owner) {
      readmeAnchors.set(anchor, route(owner.page.slug) + (owner.isTitle ? "" : `#${new GithubSlugger().slug(h.text)}`));
    }
  }
}

function resolveReadmeAnchor(hash) {
  return readmeAnchors.get(hash) ?? README_ANCHOR_FALLBACK[hash] ?? `${GH}#${hash}`;
}

function resolveLink(target, srcFile) {
  if (/^[a-z][a-z0-9+.-]*:/i.test(target) || target.startsWith("//")) return target;
  const [p, hash] = target.split("#", 2);
  if (p === "") {
    return srcFile === "README.md" ? resolveReadmeAnchor(hash) : `#${hash}`;
  }
  const repoPath = path.posix.normalize(path.posix.join(path.posix.dirname(srcFile), p)).replace(/\/$/, "");
  if (repoPath.startsWith("..")) return target;
  if (repoPath === "README.md") return hash ? resolveReadmeAnchor(hash) : "/docs/";
  const abs = path.join(REPO, repoPath);
  const isDir = fs.existsSync(abs) && fs.statSync(abs).isDirectory();
  const synced = wholeFileRoutes.get(isDir ? `${repoPath}/README.md` : repoPath);
  if (synced) return synced + (hash ? `#${hash}` : "");
  return `${GH}/${isDir ? "tree" : "blob"}/main/${repoPath}${hash ? `#${hash}` : ""}`;
}

/** Rewrites inline links outside fenced code and inline code spans. */
function rewriteLinks(lines, srcFile) {
  let fence = null;
  return lines.map((line) => {
    const f = line.match(/^\s*(```+|~~~+)/);
    if (f) {
      if (!fence) fence = f[1];
      else if (line.trim().startsWith(fence)) fence = null;
      return line;
    }
    if (fence) return line;
    return line
      .split(/(`[^`]*`)/)
      .map((part) =>
        part.startsWith("`")
          ? part
          : part.replace(/\]\(([^)\s]+)\)/g, (_, t) => `](${resolveLink(t, srcFile)})`),
      )
      .join("");
  });
}

// --- Emit -----------------------------------------------------------------

const yaml = (s) => JSON.stringify(s);

fs.rmSync(OUT, { recursive: true, force: true });
const cache = new Map();
const doc = (file) => cache.get(file) ?? cache.set(file, parse(file)).get(file);

for (const page of PAGES) {
  const body = page.sources.flatMap((s) => {
    const d = doc(s.file);
    const lines = s.intro ? extractIntro(d) : s.section ? extractSection(d, s) : extractFile(d);
    return rewriteLinks(lines, s.file);
  });
  const primary = page.sources.at(-1).file;
  const front = [
    "---",
    `title: ${yaml(page.title)}`,
    `description: ${yaml(page.description)}`,
    `editUrl: ${yaml(`${GH}/edit/main/${primary}`)}`,
    "---",
    "",
    `<!-- Generated by site/scripts/sync-docs.mjs from ${primary}. Edit the source, not this file. -->`,
    "",
  ];
  const out = path.join(OUT, page.slug ? `${page.slug}.md` : "index.md");
  fs.mkdirSync(path.dirname(out), { recursive: true });
  fs.writeFileSync(out, [...front, ...body].join("\n").replace(/\n{3,}/g, "\n\n").trimEnd() + "\n");
}

console.log(`sync-docs: wrote ${PAGES.length} pages to ${path.relative(SITE, OUT)}`);
