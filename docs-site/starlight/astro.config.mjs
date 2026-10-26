// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// https://astro.build/config
export default defineConfig({
	integrations: [
		starlight({
			title: 'navigera docs',
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/andrey-usa/navigera' }],
			sidebar: [
				{ label: 'Overview', link: '/' },
				{ label: 'Reference', link: '/reference/' },
				{ label: 'Multi-model', link: '/multi-model/' },
				{ label: 'Agent skill', link: '/skill/' },
			],
		}),
	],
});
