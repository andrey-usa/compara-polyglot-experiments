// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// https://astro.build/config
export default defineConfig({
	integrations: [
		starlight({
			title: 'Docs',
			description: 'Documentation for navigera, compara and primavera.',
			// Site a11y fixes for Starlight's own chrome (see public/a11y.js).
			head: [{ tag: 'script', attrs: { src: '/a11y.js', defer: true } }],
			social: [
				{ icon: 'github', label: 'navigera on GitHub', href: 'https://github.com/andrey-usa/navigera' },
				{ icon: 'github', label: 'compara on GitHub', href: 'https://github.com/andrey-usa/compara' },
				{ icon: 'github', label: 'primavera on GitHub', href: 'https://github.com/andrey-usa/primavera' },
			],
			sidebar: [
				{ label: 'Docs home', link: '/' },
				{
					label: 'navigera',
					items: [
						{ label: 'Overview', link: '/navigera/' },
						{ label: 'Install', link: '/navigera/install/' },
						{ label: 'Quick start', link: '/navigera/quickstart/' },
						{ label: 'Guides', link: '/navigera/guides/' },
						{ label: 'Reference', link: '/navigera/reference/' },
						{ label: 'CLI reference', link: '/navigera/cli-reference/' },
						{ label: 'Multi-model', link: '/navigera/multi-model/' },
						{ label: 'Agent skill', link: '/navigera/skill/' },
						{ label: 'llms.txt', link: '/navigera/llms/' },
						{ label: 'Changelog', link: '/navigera/changelog/' },
						{ label: 'Contributing', link: '/navigera/contributing/' },
					],
				},
				{
					label: 'compara',
					items: [
						{ label: 'Overview', link: '/compara/' },
						{ label: 'Install', link: '/compara/install/' },
						{ label: 'Quick start', link: '/compara/quickstart/' },
						{ label: 'Guides', link: '/compara/guides/' },
						{ label: 'CLI reference', link: '/compara/cli-reference/' },
						{ label: 'Report JSON contract', link: '/compara/contract/' },
						{ label: 'Changelog', link: '/compara/changelog/' },
						{ label: 'Contributing', link: '/compara/contributing/' },
					],
				},
				{
					label: 'primavera',
					items: [
						{ label: 'Overview', link: '/primavera/' },
						{ label: 'Install', link: '/primavera/install/' },
						{ label: 'Quick start', link: '/primavera/quickstart/' },
						{ label: 'Guides', link: '/primavera/guides/' },
						{ label: 'CLI reference', link: '/primavera/cli-reference/' },
						{ label: 'Agent skill', link: '/primavera/skill/' },
						{ label: 'Changelog', link: '/primavera/changelog/' },
					],
				},
			],
		}),
	],
});
