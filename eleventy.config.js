// Eleventy build for the Friring docs website.
//
// The site is plain static HTML/CSS/JS; Eleventy is used only to de-duplicate
// the shared chrome (head, nav, docs sidebar, footer) into layouts under
// website/_includes/. Each page keeps its hand-written content body and just
// declares front matter (layout, title, root depth, sidebar state).
//
// Page bodies are emitted verbatim (htmlTemplateEngine: false) so nothing in
// the content is ever interpreted as a template — only the .njk layouts run.
//
// Code blocks are the one exception: a build-time transform (`highlight-code`)
// rewrites every `<pre><code class="language-X">…</code></pre>` into a framed,
// syntax-highlighted block. Authors write plain, escaped code with a language
// class; the transform supplies the terminal chrome, language label, copy
// button, and Prism token spans. This keeps highlighting entirely at build
// time (zero client-side JS) and out of the hand-written page bodies.

import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { parse } from 'node-html-parser';
import Prism from 'prismjs';
import loadLanguages from 'prismjs/components/index.js';

loadLanguages(['bash', 'powershell', 'toml', 'json', 'rust']);

// Author-facing language tokens → the canonical Prism grammar key.
const LANG_ALIASES = {
  sh: 'bash',
  shell: 'bash',
  console: 'bash',
  bash: 'bash',
  ps: 'powershell',
  ps1: 'powershell',
  pwsh: 'powershell',
  powershell: 'powershell',
  toml: 'toml',
  json: 'json',
  rust: 'rust',
  rs: 'rust',
};

// Matches a single authored code block: an optional attribute list on <pre>
// (carrying data-title / data-wrap), then <code class="language-X">…</code>.
// The body is non-greedy and an escaped `</code>` can never appear in it, so
// this stays reliable over the controlled authoring format.
const CODE_BLOCK_RE =
  /<pre((?:\s+[a-z-]+(?:="[^"]*")?)*)\s*>\s*<code class="language-([\w-]+)">([\s\S]*?)<\/code>\s*<\/pre>/g;

// Decode HTML entities back to the raw source text Prism expects (and that the
// copy button later reads off `pre.textContent`). Reuse node-html-parser's own
// decoder by round-tripping through a throwaway element.
function decodeEntities(escaped) {
  return parse(`<x>${escaped}</x>`).querySelector('x').textContent;
}

function escapeHtml(text) {
  return text
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

// Pull data-title / data-wrap out of the captured <pre> attribute string.
function parsePreAttrs(attrs) {
  const titleMatch = attrs.match(/\bdata-title="([^"]*)"/);
  return {
    title: titleMatch ? titleMatch[1] : null,
    wrap: /\bdata-wrap\b/.test(attrs),
  };
}

// Every docs table is hand-authored as a bare `<table>` with no attributes and
// none of them nest, so a non-greedy match over the rendered HTML is reliable
// here. See the .table-scroll rule in docs.css for why they are wrapped.
const TABLE_RE = /<table>[\s\S]*?<\/table>/g;

function renderCodeBlock(match, attrs, rawLang, body) {
  const lang = LANG_ALIASES[rawLang.toLowerCase()];
  if (!lang || !Prism.languages[lang]) {
    return match; // unsupported language — leave the block untouched
  }

  const { title, wrap } = parsePreAttrs(attrs);
  const source = decodeEntities(body);
  const highlighted = Prism.highlight(source, Prism.languages[lang], lang);
  const label = title || lang;
  const wrapAttr = wrap ? ' data-wrap' : '';

  return `<div class="code-block"${wrapAttr}>
  <div class="code-block-header">
    <span class="terminal-dot red"></span>
    <span class="terminal-dot yellow"></span>
    <span class="terminal-dot green"></span>
    <span class="code-block-lang">${escapeHtml(decodeEntities(label))}</span>
    <button class="copy-btn" type="button" aria-label="Copy code to clipboard">Copy</button>
  </div>
  <div class="code-block-body"><pre><code class="language-${lang}">${highlighted}</code></pre></div>
</div>`;
}

// ---- Build-time CSS bundle ----
// The four sheets every page loads (tokens, reset, layout, components) are
// concatenated into one `core.css`, turning four render-blocking requests into
// one. They stay authored separately for maintainability and must keep this
// order — variables first (every other file reads its custom properties), then
// base, layout, components, matching the old <link> order exactly so the
// cascade is unchanged. The page-specific sheets are deliberately NOT bundled:
// that would ship landing CSS to docs pages and vice versa.
const CORE_CSS = ['variables', 'base', 'layout', 'components'];
const PAGE_CSS = ['landing', 'docs', 'ui-review'];

// Comments and blank lines are ~20% of the source and carry no runtime meaning.
// Deliberately conservative: strip /* … */ and collapse runs of blank lines, but
// leave declarations untouched — a real minifier is not worth a dependency here,
// and mangling values would risk the layout for a few hundred bytes.
function stripCssComments(css) {
  return css.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\n{2,}/g, '\n');
}

function writeCoreBundle() {
  const parts = CORE_CSS.map((name) => {
    const src = readFileSync(`website/css/${name}.css`, 'utf8');
    return `/* ${name}.css */\n${stripCssComments(src).trim()}`;
  });
  const bundle = `/* Generated by eleventy.config.js — edit website/css/*.css, not this file. */\n${parts.join('\n')}\n`;
  mkdirSync('_site/css', { recursive: true });
  writeFileSync('_site/css/core.css', bundle);
  return bundle.length;
}

export default function (eleventyConfig) {
  // Runs on every build and rebuild, so `--serve` keeps the bundle fresh.
  eleventyConfig.on('eleventy.after', () => {
    const bytes = writeCoreBundle();
    console.log(`[11ty] Wrote _site/css/core.css (${(bytes / 1024).toFixed(1)} KB)`);
  });

  // Static assets are copied through untouched. The input-dir prefix
  // ("website/") is stripped in the output, so these land at _site/css, etc.
  // Only the page-specific sheets are copied: the four core files ship as the
  // generated core.css bundle instead, so copying them too would be dead weight
  // in the deployed output.
  for (const sheet of PAGE_CSS) {
    eleventyConfig.addPassthroughCopy(`website/css/${sheet}.css`);
  }
  eleventyConfig.addPassthroughCopy('website/js');
  eleventyConfig.addPassthroughCopy('website/assets');
  // robots.txt (crawler discovery) + llms.txt (curated entry point for
  // LLM-based engines and coding agents). Same prefix-stripping as above.
  // No CNAME: the fork serves from the default bvc3at.github.io/friring rather
  // than a custom domain, and a CNAME in the artifact would make
  // actions/deploy-pages claim that hostname instead.
  eleventyConfig.addPassthroughCopy('website/robots.txt');
  eleventyConfig.addPassthroughCopy('website/llms.txt');

  // Build-time syntax highlighting. Runs on rendered HTML output only, so the
  // page bodies stay verbatim in source. Blocks without a `language-*` class
  // (the ASCII-art TUI mockups, video frames) never match and are untouched.
  eleventyConfig.addTransform('highlight-code', function (content, outputPath) {
    if (!outputPath || !outputPath.endsWith('.html')) return content;
    return content.replace(CODE_BLOCK_RE, renderCodeBlock);
  });

  // Give every docs table its own horizontal scroll box, so a wide reference
  // table scrolls on its own instead of dragging the whole page sideways.
  eleventyConfig.addTransform('wrap-tables', function (content, outputPath) {
    if (!outputPath || !outputPath.endsWith('.html')) return content;
    return content.replace(TABLE_RE, (table) => `<div class="table-scroll">${table}</div>`);
  });

  return {
    dir: {
      input: 'website',
      output: '_site',
      includes: '_includes',
    },
    // Layouts are Nunjucks; page bodies are left untouched. `njk` is enabled
    // for generated data files (sitemap.xml); `_includes` is excluded from
    // page processing, so the shared layouts are unaffected.
    htmlTemplateEngine: false,
    templateFormats: ['html', 'njk'],
  };
}
