import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  output: 'export',
  reactStrictMode: true,
  // GitHub Pages serves the site under https://rodcor.github.io/kimetsu
  basePath: '/kimetsu',
  trailingSlash: true,
  images: { unoptimized: true },
};

export default withMDX(config);
