import assert from 'node:assert';
import * as sideEffect from './side-effect.js';
import req from './req.cjs';

const keys = (value) => (value === undefined ? 'UNDEFINED' : Object.keys(value).sort().join(','));

// A file with no module syntax is scanned as `ExportsKind::None` - "no importer has
// spoken yet", not "has no exports". Importing it settles it as ESM, so its namespace
// exists and is empty; `require`ing it settles it as CommonJS.
assert.strictEqual(keys(sideEffect), '', 'namespace of an imported no-syntax module');
assert.strictEqual(keys(req), 'keys', 'the CommonJS bridge itself');
assert.strictEqual(req.keys, '', 'exports of a required no-syntax module');

// The side effects themselves still run, in both the eager bundle and the wrappers.
assert.match(globalThis.__sideEffect, /^v[01]$/);
assert.match(globalThis.__required, /^v[01]$/);

import.meta.hot.accept();
