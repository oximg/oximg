import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import { oxideTheme } from "./src/styles/code-theme.ts";

export default defineConfig({
  site: "https://oximg.dev",
  // Everything is static; islands opt in to JS individually.
  output: "static",
  // Astro's compressor drops the line break between text and an inline
  // element that starts the next source line ("in<a>BENCH.md</a>"),
  // which runs words together all over the prose.
  compressHTML: false,
  build: {
    inlineStylesheets: "always",
  },
  integrations: [
    // Serves /docs/*. Pages are generated from the repo's markdown by
    // scripts/sync-docs.mjs; the home page stays a plain Astro page.
    starlight({
      title: "oximg",
      description: "High-performance image resizing in Rust: server, CLI and library.",
      logo: { src: "./src/assets/logo.svg" },
      favicon: "/favicon.svg",
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/oximg/oximg" }],
      customCss: ["./src/styles/starlight.css"],
      expressiveCode: {
        themes: [oxideTheme],
        styleOverrides: { borderRadius: "8px" },
      },
      sidebar: [
        {
          label: "Start",
          items: [
            { label: "Overview", link: "/docs/" },
            { label: "Install", slug: "docs/install" },
          ],
        },
        {
          label: "Guides",
          items: [
            { label: "Serving", slug: "docs/serving" },
            { label: "Formats", slug: "docs/formats" },
            { label: "CLI", slug: "docs/cli" },
            { label: "Rust library", slug: "docs/library" },
          ],
        },
        {
          label: "Reference",
          items: [
            { label: "Configuration", slug: "docs/configuration" },
            { label: "Errors", slug: "docs/errors" },
            { label: "Pipeline", slug: "docs/pipeline" },
          ],
        },
        {
          label: "Deploy",
          items: [
            { label: "Docker", slug: "docs/deploy/docker" },
            { label: "Kubernetes", slug: "docs/deploy/kubernetes" },
            { label: "Cloud Run", slug: "docs/deploy/cloud-run" },
          ],
        },
        {
          label: "Ruby",
          items: [
            { label: "oximg gem", slug: "docs/ruby/oximg" },
            { label: "oximg-rails", slug: "docs/ruby/rails" },
          ],
        },
      ],
    }),
  ],
});
