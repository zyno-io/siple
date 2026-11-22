// @ts-check
import { defineConfig } from 'astro/config';

// Siple marketing site: static, one page plus a 404. The docs live at
// https://zyno-io.github.io/siple/ and the code at github.com/zyno-io/siple.
//
// `site` sets the canonical URL, Open Graph URLs and the sitemap; robots.txt,
// src/data/seo.ts and .github/workflows/www.yml name the same domain.
export default defineConfig({
  site: 'https://siple.app',
});
