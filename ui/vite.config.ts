import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

const root = fileURLToPath(new URL(".", import.meta.url));

/**
 * Static, dependency-free build for the embedded read-only dashboard.
 *
 * `base: "./"` keeps every asset URL relative so the Rust server can mount the
 * bundle at `/`, `/ui/` or anything else without rebuilding. `dist/` is committed
 * on purpose: `cargo build` embeds it and never needs node.
 */
export default defineConfig({
	root,
	base: "./",
	build: {
		outDir: "dist",
		emptyOutDir: true,
		assetsDir: "assets",
		target: "es2022",
		sourcemap: false,
		reportCompressedSize: false,
	},
	server: {
		port: 5174,
		strictPort: true,
		// Dev-only convenience: `apm ui` serves the API on loopback.
		proxy: { "/api": "http://127.0.0.1:7717" },
	},
});
