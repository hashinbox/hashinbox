// Lint for the server's browser app. Besides the usual correctness rules, it
// holds the folder structure: generic components know no domain, a page never
// reaches into another page, and nothing but src/api talks HTTP.
import js from '@eslint/js';
import reactHooks from 'eslint-plugin-react-hooks';
import reactRefresh from 'eslint-plugin-react-refresh';
import prettier from 'eslint-config-prettier';
import globals from 'globals';
import tseslint from 'typescript-eslint';

// Imports reach across folders through the `@/` alias; `../` is never used,
// so every boundary below is a rule about an `@/` path.
const parent = { regex: '^\\.\\./', message: 'Import through `@/`, not `../`.' };

function imports(files, ...patterns) {
  return {
    files: [files],
    rules: { 'no-restricted-imports': ['error', { patterns: [parent, ...patterns] }] },
  };
}

export default tseslint.config(
  { ignores: ['dist', 'src/api/schema.d.ts'] },
  js.configs.recommended,
  ...tseslint.configs.strictTypeChecked,
  ...tseslint.configs.stylisticTypeChecked,
  // Formatting is Prettier's; turn off every ESLint rule that disagrees with it.
  prettier,
  {
    files: ['**/*.{ts,tsx}'],
    languageOptions: {
      globals: globals.browser,
      parserOptions: { projectService: true, tsconfigRootDir: import.meta.dirname },
    },
    plugins: { 'react-hooks': reactHooks, 'react-refresh': reactRefresh },
    rules: {
      ...reactHooks.configs.recommended.rules,
      // `onClick={() => setOpen(true)}` is how React handlers are written.
      '@typescript-eslint/no-confusing-void-expression': ['error', { ignoreArrowShorthand: true }],
      '@typescript-eslint/restrict-template-expressions': ['error', { allowNumber: true }],
      'react-refresh/only-export-components': ['error', { allowExportNames: [] }],
      'no-restricted-globals': [
        'error',
        { name: 'fetch', message: 'Talk to the server through the generated client in src/api.' },
      ],
    },
  },
  imports('src/**'),
  // Generic components carry no domain: no pages, no API, no amounts.
  imports('src/_components/**', {
    group: ['@/pages/**', '@/api/**', '@/money'],
    message: 'A generic component knows no domain.',
  }),
  // A page uses its own files (`./`) and shared ones (`@/`), never another page.
  imports('src/pages/**', { group: ['@/pages/**'], message: 'A page never imports another page.' }),
  {
    // The API client is the only place that performs a request.
    files: ['src/api/**'],
    rules: { 'no-restricted-globals': 'off' },
  },
  {
    files: ['eslint.config.js', 'vite.config.ts'],
    ...tseslint.configs.disableTypeChecked,
  },
);
