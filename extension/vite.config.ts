/// <reference types="vitest/config" />
import { copyFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { defineConfig, type Plugin } from 'vite';

// Content scripts are classic scripts, not modules, so each entry builds on
// its own into one self-contained IIFE with no shared chunks. `npm run build`
// runs one `vite build --mode <entry>` per entry; the mode also gates each
// entry's start-up call (`import.meta.env.MODE`), which is 'test' under Vitest.
const ENTRIES = ['hook', 'content', 'worker'];

const VIRTUAL_ENTRY = '\0recall-entry';

// The bundle's entry is a virtual module that imports src/<mode>.ts for its
// side effects only. The source files export functions for the tests; built
// straight from them, the IIFE would publish those exports as a global
// variable, and hook.js runs in Meet's own page.
function sideEffectEntry(mode: string): Plugin {
  return {
    name: 'side-effect-entry',
    resolveId: (id) => (id === VIRTUAL_ENTRY ? id : null),
    load: (id) => (id === VIRTUAL_ENTRY ? `import ${JSON.stringify(resolve(`src/${mode}.ts`))};` : null),
  };
}

function copyManifest(): Plugin {
  return {
    name: 'copy-manifest',
    closeBundle() {
      copyFileSync('manifest.json', 'dist/manifest.json');
    },
  };
}

export default defineConfig(({ mode }) =>
  ENTRIES.includes(mode)
    ? {
        publicDir: false,
        plugins: [sideEffectEntry(mode), ...(mode === 'hook' ? [copyManifest()] : [])],
        build: {
          outDir: 'dist',
          // The first build clears dist; the others add to it.
          emptyOutDir: mode === 'hook',
          minify: false,
          target: 'es2022',
          rollupOptions: {
            input: VIRTUAL_ENTRY,
            output: { format: 'iife', entryFileNames: `${mode}.js` },
          },
        },
      }
    : {
        test: {
          include: ['src/**/*.test.ts'],
          environment: 'jsdom',
        },
      },
);
