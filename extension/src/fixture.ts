import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

/** Loads `test/fixtures/<name>.html` into the jsdom body. Vitest runs from the package root. */
export function loadFixture(name: string): void {
  document.body.innerHTML = readFileSync(resolve('test/fixtures', `${name}.html`), 'utf8');
}
