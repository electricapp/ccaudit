// ESLint flat config (v9+). Scopes to the web view's standalone JS so
// the rest of the repo (Rust source, configs) is ignored. The JS runs
// in the browser against a synchronously-loaded global state — strict
// mode is on by default in module scripts but our app.js is loaded as
// a classic <script>, so we declare globals explicitly via `sourceType:
// 'script'` and the `globals` package.
import globals from 'globals';

export default [
  {
    ignores: ['target/**', 'node_modules/**', 'npm/**'],
  },
  {
    files: ['src/web/app.js'],
    languageOptions: {
      ecmaVersion: 2024,
      sourceType: 'script',
      globals: {
        ...globals.browser,
        // Key rebinds emitted by web.rs ahead of this script. Absent
        // when app.js is opened straight from disk, hence the `typeof`
        // guard at its only use.
        CCAUDIT_REBIND: 'readonly',
        // Functions defined in src/web/util.js. That file is
        // concatenated ahead of app.js in the generated script block,
        // so these end up as runtime globals — eslint needs to be told.
        slugify: 'readonly',
        ft: 'readonly',
        fc: 'readonly',
        fd: 'readonly',
        esc: 'readonly',
        fdur: 'readonly',
        durMs: 'readonly',
        hl: 'readonly',
        dn: 'readonly',
        dayStr: 'readonly',
        emptyDay: 'readonly',
        tokenParts: 'readonly',
        costParts: 'readonly',
        tokTip: 'readonly',
        costTip: 'readonly',
        tokenize: 'readonly',
        lowerBound: 'readonly',
        shq: 'readonly',
        resumeCmd: 'readonly',
      },
    },
    rules: {
      // Functions called only from inline `onclick="…"` handlers in the
      // HTML strings the JS itself emits look unused to a static linter.
      // The pattern is the convention in this file: short opener names
      // (`o*` = open, `r*` = render) and the named UI handlers below.
      'no-unused-vars': [
        'warn',
        {
          args: 'none',
          caughtErrors: 'none',
          varsIgnorePattern:
            '^(_|o[A-Z]|r[A-Z]|crumb|setDateRange|setDetail|setHistMode|setPieMode|toggleHistLog|toggleKind|toggleCompact|setSingleDay|goBack)',
        },
      ],
      'no-undef': 'error',
      eqeqeq: ['warn', 'smart'],
      'no-var': 'error',
      'prefer-const': 'warn',
      'no-empty': ['warn', { allowEmptyCatch: true }],
    },
  },
];
