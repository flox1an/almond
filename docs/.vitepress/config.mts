import { defineConfig } from 'vitepress'

export default defineConfig({
  title: 'Almond',
  // Served from https://flox1an.github.io/almond/ (GitHub Pages project site).
  base: '/almond/',
  description: 'A Blossom media server for Nostr',
  cleanUrls: true,
  lastUpdated: true,
  // Agent tooling notes, not user documentation.
  srcExclude: ['agents/**'],
  themeConfig: {
    nav: [
      { text: 'Guide', link: '/guide/getting-started' },
      { text: 'Configuration', link: '/configuration/' },
      { text: 'Rust library', link: '/library' },
      { text: 'Blossom specs', link: '/blossom/' },
      { text: 'API', link: '/reference/api' },
    ],
    sidebar: [
      {
        text: 'Guide',
        items: [
          { text: 'Getting started', link: '/guide/getting-started' },
          { text: 'How it works', link: '/guide/how-it-works' },
          { text: 'Development', link: '/guide/development' },
        ],
      },
      {
        text: 'Configuration',
        items: [
          { text: 'Overview', link: '/configuration/' },
          { text: 'Command-line flags', link: '/configuration/cli' },
          { text: 'Environment variables', link: '/configuration/environment' },
        ],
      },
      {
        text: 'Deployment',
        items: [
          { text: 'Binary and systemd', link: '/deployment/systemd' },
          { text: 'Docker', link: '/deployment/docker' },
          { text: 'HTTPS', link: '/deployment/https' },
          { text: 'S3 providers', link: '/deployment/s3' },
          { text: 'Local Blossom Cache', link: '/deployment/local-cache' },
          { text: 'FIPS mesh image', link: '/deployment/fips' },
        ],
      },
      { text: 'Rust library', items: [{ text: 'Embedding in Axum', link: '/library' }] },
      {
        text: 'Blossom specs',
        items: [
          { text: 'Support matrix', link: '/blossom/' },
          { text: 'Spec deviations', link: '/blossom/compliance' },
          { text: 'Chunked uploads', link: '/blossom/chunked-upload' },
          { text: 'Blob discovery filter', link: '/blossom/filter' },
          { text: 'Filter JS decoding', link: '/blossom/filter-js-spec' },
        ],
      },
      { text: 'Reference', items: [{ text: 'HTTP API', link: '/reference/api' }] },
    ],
    socialLinks: [{ icon: 'github', link: 'https://github.com/flox1an/almond' }],
    editLink: {
      pattern: 'https://github.com/flox1an/almond/edit/main/docs/:path',
    },
    search: { provider: 'local' },
    footer: { message: 'Released under the MIT License.' },
  },
})
