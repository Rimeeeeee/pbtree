import test from 'node:test';
import assert from 'node:assert/strict';
import { renderReport } from './benchmark-report.mjs';

const options = { repository: 'Rimeeeeee/pbtree', headSha: 'a'.repeat(40), runUrl: 'https://github.com/Rimeeeeee/pbtree/actions/runs/1' };
const row = (id, mean, low, high) => ({ id, mean_ns: mean, low_ns: low, high_ns: high });
const run = (commit, benchmarks) => ({ commit, suite_hash: 'b'.repeat(64), toolchain: 'rustc 1.96.0', benchmarks });
const payload = (base, head) => ({ schema: 1, base: run('c'.repeat(40), base), head: run(options.headSha, head) });

test('flags meaningful regressions and improvements, and leaves noisy deltas advisory', () => {
  const data = payload(
    [row('slow', 100, 99, 101), row('fast', 100, 99, 101), row('noise', 100, 90, 110), row('small', 100, 99, 101)],
    [row('slow', 120, 119, 121), row('fast', 80, 79, 81), row('noise', 108, 98, 118), row('small', 104, 103, 105)],
  );
  const report = renderReport(data, options);
  assert.match(report, /1 potential regressions · 1 improvements/);
  assert.match(report, /\+20.0% — ⚠️ regression/);
  assert.match(report, /-20.0% — ✅ improvement/);
  assert.match(report, /\+8.0% — within threshold \/ noise/);
  assert.match(report, /\+4.0% — within threshold \/ noise/);
});

test('harness changes, added/removed cases and bootstrap do not invent a speed comparison', () => {
  const data = payload([row('old', 100, 99, 101), row('shared', 100, 99, 101)], [row('new', 80, 79, 81), row('shared', 80, 79, 81)]);
  data.head.suite_hash = 'd'.repeat(64);
  const report = renderReport(data, options);
  assert.match(report, /Harness\/toolchain changed/);
  assert.match(report, /Removed benchmarks: `old`/);
  assert.doesNotMatch(report, /✅ improvement/);
  assert.match(renderReport({ ...data, base: null }, options), /Initial benchmark baseline/);
});

test('rejects stale, malformed and non-finite results before publishing', () => {
  const data = payload([], [row('valid', 100, 99, 101)]);
  data.base = null;
  assert.throws(() => renderReport(data, { ...options, headSha: 'e'.repeat(40) }), /another commit/);
  data.head.benchmarks[0].mean_ns = Infinity;
  assert.throws(() => renderReport(data, options), /estimate/);
  data.head.benchmarks = [row('name\n| injected', 100, 99, 101)];
  assert.throws(() => renderReport(data, options), /benchmark ID/);
});

test('adding a backend suite does not disable comparisons for unchanged harnesses', () => {
  const data = payload([row('tree/op', 100, 99, 101)], [row('tree/op', 80, 79, 81), row('backends/new', 50, 49, 51)]);
  data.base.benchmarks[0].source_hash = 'f'.repeat(64);
  data.head.benchmarks[0].source_hash = 'f'.repeat(64);
  data.head.suite_hash = 'e'.repeat(64);
  assert.match(renderReport(data, options), /0 potential regressions · 1 improvements/);
});
