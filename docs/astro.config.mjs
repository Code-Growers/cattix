import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
	site: 'https://code-growers.github.io',
	base: '/cattix',
	integrations: [
		starlight({
			title: 'Cattix',
			description: 'Fleet management and health-gated rolling deployments for NixOS.',
			sidebar: [],
		}),
	],
});
