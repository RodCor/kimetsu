// @ts-check
import {themes as prismThemes} from 'prism-react-renderer';

/** @type {import('@docusaurus/types').Config} */
const config = {
  title: 'Kimetsu',
  tagline: 'Proactive memory for AI coding agents',
  favicon: 'img/favicon.ico',

  future: {
    v4: true,
  },

  url: 'https://rodcor.github.io',
  baseUrl: '/kimetsu/',
  trailingSlash: false,

  organizationName: 'RodCor',
  projectName: 'kimetsu',

  onBrokenLinks: 'warn',

  markdown: {
    format: 'detect',
    hooks: {
      onBrokenMarkdownLinks: 'warn',
    },
  },

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  presets: [
    [
      'classic',
      /** @type {import('@docusaurus/preset-classic').Options} */
      ({
        docs: {
          sidebarPath: './sidebars.js',
          // Point at the repo's docs folder (one level up from website/)
          path: '../docs-site-content',
          editUrl: 'https://github.com/RodCor/kimetsu/edit/main/',
          exclude: ['superpowers/**', '**/superpowers/**'],
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      }),
    ],
  ],

  themeConfig:
    /** @type {import('@docusaurus/preset-classic').ThemeConfig} */
    ({
      image: 'img/kimetsu-logo.png',
      colorMode: {
        defaultMode: 'dark',
        respectPrefersColorScheme: false,
      },
      navbar: {
        title: 'Kimetsu',
        logo: {
          alt: 'Kimetsu logo',
          src: 'img/kimetsu-logo.png',
        },
        items: [
          {
            type: 'docSidebar',
            sidebarId: 'docsSidebar',
            position: 'left',
            label: 'Docs',
          },
          {
            href: 'https://github.com/RodCor/kimetsu',
            label: 'GitHub',
            position: 'right',
          },
          {
            href: 'https://crates.io/crates/kimetsu-cli',
            label: 'crates.io',
            position: 'right',
          },
          {
            href: 'https://www.npmjs.com/package/kimetsu-ai',
            label: 'npm',
            position: 'right',
          },
        ],
      },
      footer: {
        style: 'dark',
        links: [
          {
            title: 'Docs',
            items: [
              {label: 'Introduction', to: '/docs/intro'},
              {label: 'Install', to: '/docs/install'},
              {label: 'How it works', to: '/docs/how-kimetsu-works'},
              {label: 'Benchmark', to: '/docs/memory-benchmark'},
              {label: 'Changelog', to: '/docs/changelog'},
            ],
          },
          {
            title: 'Links',
            items: [
              {
                label: 'GitHub',
                href: 'https://github.com/RodCor/kimetsu',
              },
              {
                label: 'crates.io',
                href: 'https://crates.io/crates/kimetsu-cli',
              },
              {
                label: 'npm',
                href: 'https://www.npmjs.com/package/kimetsu-ai',
              },
            ],
          },
        ],
        copyright: `Copyright © ${new Date().getFullYear()} Kimetsu contributors. Built with Docusaurus.`,
      },
      prism: {
        theme: prismThemes.github,
        darkTheme: prismThemes.dracula,
        additionalLanguages: ['bash', 'toml', 'rust'],
      },
    }),
};

export default config;
