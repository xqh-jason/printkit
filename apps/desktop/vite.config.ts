import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  // `@printkit/i18n` peers on Vue; keep a single instance in the bundle.
  resolve: {
    dedupe: ["vue"],
  },
  server: {
    port: 1420,
    strictPort: true,
    // Tauri's CSP only covers the tauri:// asset protocol; dev pages load
    // from Vite over HTTP, so the same policy (plus HMR's websocket and the
    // dev-time inline/eval scripts) is injected here.
    headers: {
      "Content-Security-Policy": [
        "default-src 'self'",
        "script-src 'self' 'unsafe-inline' 'unsafe-eval'",
        "style-src 'self' 'unsafe-inline'",
        "img-src 'self' data:",
        "connect-src 'self' ws://localhost:1420 http://localhost:1420",
        "object-src 'none'",
      ].join("; "),
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "safari15",
    manifest: true,
    // Let Rollup split on real dynamic imports: a manualChunks entry for the
    // `element-plus` root would keep every re-export in one chunk.
    chunkSizeWarningLimit: 700,
  },
});
