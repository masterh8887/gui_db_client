/** @type {import('next').NextConfig} */
module.exports = {
  output: 'export', // static files in out/; Tauri loads them. No SSR, no API routes.
  images: { unoptimized: true },
  trailingSlash: true,
  reactStrictMode: true,
  // Lint runs separately (`pnpm --filter desktop lint`); Next 16 no longer lints during build.
  // Tauri dev server: the WebView loads http://localhost:3000; nothing else is allowed.
  devIndicators: false,
};
