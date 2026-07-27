import assert from 'node:assert';
import req from './req.cjs';
import importedDefault from './imported.json';
import * as importedNs from './imported.json';
import * as bothNs from './both.json';

const keys = (value) => Object.keys(value).sort().join(',');

// `require` settles a JSON module on `module.exports`, so it sees the payload itself.
assert.strictEqual(req.requiredKeys, 'name,nested', 'require-only JSON');

// A plain `import` leaves it ESM: a namespace, `default` included.
assert.strictEqual(keys(importedNs), 'default,name,nested', 'import-only JSON namespace');
assert.strictEqual(keys(importedDefault), 'name,nested', 'import-only JSON default');

// Required by one module and imported by another: `require` wins and the module becomes
// CommonJS, so the ESM side reaches it through interop rather than a real namespace.
assert.strictEqual(req.bothKeys, 'name,nested', 'required side of a both-ways JSON');
assert.strictEqual(keys(bothNs.default), 'name,nested', 'imported side of a both-ways JSON');

import.meta.hot.accept();
