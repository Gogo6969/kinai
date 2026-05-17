import { Marked } from 'marked';
import { markedHighlight } from 'marked-highlight';
import hljs from 'highlight.js';
import katex from 'katex';

const marked = new Marked(
  markedHighlight({
    emptyLangClass: 'hljs',
    langPrefix: 'hljs language-',
    highlight(code, lang) {
      const language = hljs.getLanguage(lang) ? lang : 'plaintext';
      return hljs.highlight(code, { language }).value;
    },
  })
);

marked.setOptions({ gfm: true, breaks: true });

// Custom image renderer: enforce a safe URL allow-list and a max-height
// box. Without this a model can drop a `![…](javascript:…)` or a 50 MP
// image into chat and either pop XSS or blow up the layout. We accept
// `https://`, `http://` (LAN-only resources from the host), and `data:`
// for inline previews / vision uploads — and that's it.
marked.use({
  renderer: {
    image(token: { href?: string; title?: string | null; text?: string }) {
      const raw = token.href ?? '';
      const alt = (token.text ?? '').replace(/"/g, '&quot;');
      const titleAttr = token.title
        ? ` title="${token.title.replace(/"/g, '&quot;')}"`
        : '';
      if (!isSafeImageUrl(raw)) {
        // Render as a plain link instead of a broken/dangerous img.
        return `<a href="#" class="text-red-300 underline-offset-2 underline" data-blocked-image="${raw}">[blocked image: ${alt || 'untitled'}]</a>`;
      }
      const href = raw.replace(/"/g, '&quot;');
      // Lazy + decoded async so a chat with 20 images doesn't stall
      // first paint. Wrap in a kin-img-open link so a single click
      // routes through the shell plugin and opens the image in the
      // user's default viewer (Preview on macOS, Photos on Windows).
      // Tauri's WebView blocks the built-in right-click → "Open Image
      // in New Window" silently, so we provide a reliable click-to-open.
      // data: URLs (inline-embedded images) skip the wrapper — they
      // can't be opened externally and the cursor-pointer would be a
      // lie.
      const isData = raw.startsWith('data:');
      const img = `<img src="${href}" alt="${alt}"${titleAttr} class="kin-img" loading="lazy" decoding="async" />`;
      if (isData) return img;
      return `<a href="${href}" class="kin-img-open" title="Click to open in default image viewer" data-kin-open="${href}">${img}</a>`;
    },
  },
});

function isSafeImageUrl(url: string): boolean {
  const trimmed = url.trim();
  if (!trimmed) return false;
  // data: URLs — only image/* MIME types.
  if (trimmed.startsWith('data:')) {
    return /^data:image\/(png|jpeg|jpg|gif|webp|svg\+xml|x-icon);base64,/i.test(trimmed);
  }
  // Block schemes that can execute or escape the sandbox.
  if (/^(javascript|file|vbscript|about):/i.test(trimmed)) return false;
  // Otherwise we accept https/http and trust the CSS to constrain size.
  return /^https?:\/\//i.test(trimmed);
}

/** Render markdown + LaTeX. KaTeX is applied before marked so $$…$$ blocks
 *  don't get mangled by GFM. */
export function renderMarkdown(src: string): string {
  const withMath = renderMath(src);
  return marked.parse(withMath) as string;
}

function renderMath(src: string): string {
  // Display math: $$ … $$
  let out = src.replace(/\$\$([\s\S]+?)\$\$/g, (_, body) => {
    try {
      return katex.renderToString(body.trim(), { displayMode: true, throwOnError: false });
    } catch {
      return _;
    }
  });
  // Inline math: $ … $ (no leading digit-then-$ to avoid USD false positives)
  out = out.replace(/(^|[^\d])\$([^$\n]+?)\$(?!\d)/g, (_full, lead, body) => {
    try {
      return lead + katex.renderToString(body.trim(), { displayMode: false, throwOnError: false });
    } catch {
      return _full;
    }
  });
  return out;
}
