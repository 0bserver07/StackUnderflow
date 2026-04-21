// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// https://astro.build/config
export default defineConfig({
  site: 'https://0bserver07.github.io',
  base: '/StackUnderflow',
  integrations: [
    starlight({
      title: 'StackUnderflow',
      description: 'A local-first knowledge base for your AI coding sessions across Claude Code, Codex, and other tools.',
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/0bserver07/StackUnderflow' },
      ],
      editLink: {
        baseUrl: 'https://github.com/0bserver07/StackUnderflow/edit/main/docs-site/',
      },
      sidebar: [
        {
          label: 'Getting started',
          items: [
            { label: 'Installation', link: '/installation/' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'CLI', link: '/cli-reference/' },
            { label: 'HTTP API', link: '/api-reference/' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Development', link: '/dev-guide/' },
            { label: 'Tests', link: '/tests/' },
          ],
        },
        {
          label: 'Internals',
          items: [
            { label: 'Claude log format', link: '/internals/logs/' },
            { label: 'Performance', link: '/internals/performance/' },
          ],
        },
      ],
    }),
  ],
});
