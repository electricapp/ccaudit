// Headless test runner for src/web/util.js.
//
// Runs the same test.js suite that test.html does, but under Node with
// a minimal DOM shim so CI can verify the util functions without a
// browser. No deps — Node's stdlib only.
//
//   node src/web/run-tests.js
//
// Exits non-zero on any failure so it's usable as a Makefile / CI step.

const fs = require('fs');
const path = require('path');

let failed = 0;
const logs = [];

global.document = {
  getElementById: (id) => ({
    set textContent(v) {
      this._t = v;
      if (id === 'summary') logs.push('\n' + v);
    },
    get textContent() {
      return this._t;
    },
    set className(v) {
      this._c = v;
      if (id === 'summary' && v === 'fail') failed = 1;
    },
    appendChild(c) {
      if (c._class === 'suite') logs.push('\n' + c._text.toUpperCase());
      else {
        logs.push(c._text);
        if ((c._class || '').includes('fail')) failed = 1;
      }
    },
  }),
  createElement: () => ({
    set textContent(v) {
      this._text = v;
    },
    get textContent() {
      return this._text;
    },
    set className(v) {
      this._class = v;
    },
    get className() {
      return this._class;
    },
  }),
};

const here = path.dirname(__filename);
const util = fs.readFileSync(path.join(here, 'util.js'), 'utf8');
const tests = fs.readFileSync(path.join(here, 'test.js'), 'utf8');
eval(util); // eslint-disable-line no-eval
eval(tests); // eslint-disable-line no-eval

console.log(logs.join('\n'));
process.exit(failed);
