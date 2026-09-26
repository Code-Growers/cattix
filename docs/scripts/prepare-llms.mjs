import { mkdir, readFile, writeFile } from 'node:fs/promises';

const source = new URL('../src/content/docs/index.md', import.meta.url);
const publicDir = new URL('../public/', import.meta.url);
const llmsFile = new URL('../public/llms.txt', import.meta.url);

await mkdir(publicDir, { recursive: true });
const markdown = await readFile(source, 'utf8');
const withoutFrontmatter = markdown.replace(/^---\r?\n[\s\S]*?\r?\n---\r?\n/, '');
await writeFile(llmsFile, withoutFrontmatter);
