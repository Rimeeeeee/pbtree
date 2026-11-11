export const marker = '<!-- pbt-benchmark-report -->';

function validateRun(run) {
  if (!run || !/^[0-9a-f]{40}$/.test(run.commit) || !/^[0-9a-f]{64}$/.test(run.suite_hash)) {
    throw new Error('Invalid benchmark provenance');
  }
  if (typeof run.toolchain !== 'string' || !/^rustc [\w .()-]{1,180}$/.test(run.toolchain)) {
    throw new Error('Invalid benchmark toolchain');
  }
  if (!Array.isArray(run.benchmarks) || run.benchmarks.length < 1 || run.benchmarks.length > 500) {
    throw new Error('Invalid benchmark count');
  }
  const ids = new Set();
  for (const row of run.benchmarks) {
    if (typeof row.id !== 'string' || !/^[\w./ ():-]{1,200}$/.test(row.id) || ids.has(row.id)) {
      throw new Error('Invalid or duplicate benchmark ID');
    }
    ids.add(row.id);
    if (row.source_hash !== undefined && !/^[0-9a-f]{64}$/.test(row.source_hash)) throw new Error('Invalid benchmark source hash');
    for (const value of [row.low_ns, row.mean_ns, row.high_ns]) {
      if (!Number.isFinite(value) || value <= 0) throw new Error('Invalid benchmark estimate');
    }
    if (!(row.low_ns <= row.mean_ns && row.mean_ns <= row.high_ns)) throw new Error('Invalid interval');
  }
}

export function duration(ns) {
  if (ns >= 1e9) return `${(ns / 1e9).toPrecision(3)} s`;
  if (ns >= 1e6) return `${(ns / 1e6).toPrecision(3)} ms`;
  if (ns >= 1e3) return `${(ns / 1e3).toPrecision(3)} µs`;
  return `${ns.toPrecision(3)} ns`;
}

export function renderReport(payload, { repository, headSha, runUrl }) {
  if (payload.schema !== 1) throw new Error('Unsupported benchmark schema');
  validateRun(payload.head);
  if (payload.head.commit !== headSha) throw new Error('Benchmark results belong to another commit');
  if (payload.base) validateRun(payload.base);
  const base = new Map((payload.base?.benchmarks ?? []).map(row => [row.id, row]));
  const sameToolchain = payload.base && payload.base.toolchain === payload.head.toolchain;
  let comparable = 0;
  let regressions = 0;
  let improvements = 0;
  const rows = payload.head.benchmarks.map(row => {
    const previous = base.get(row.id);
    let change = 'New';
    const sameHarness = previous && (previous.source_hash ?? payload.base.suite_hash) === (row.source_hash ?? payload.head.suite_hash);
    if (previous && sameHarness && sameToolchain) {
      comparable += 1;
      const delta = (row.mean_ns / previous.mean_ns - 1) * 100;
      const regression = delta > 5 && row.low_ns > previous.high_ns;
      const improvement = delta < -5 && row.high_ns < previous.low_ns;
      regressions += Number(regression);
      improvements += Number(improvement);
      change = `${delta >= 0 ? '+' : ''}${delta.toFixed(1)}% — ${regression ? '⚠️ regression' : improvement ? '✅ improvement' : 'within threshold / noise'}`;
    } else if (previous) {
      change = 'Harness/toolchain changed';
    } else if (!payload.base) {
      change = 'Initial baseline';
    }
    return `| \`${row.id}\` | ${previous ? duration(previous.mean_ns) : '—'} | ${duration(row.mean_ns)} | ${change} |`;
  });
  const removed = [...base.keys()].filter(id => !payload.head.benchmarks.some(row => row.id === id));
  const summary = comparable > 0
    ? `**${regressions} potential regressions · ${improvements} improvements**`
    : payload.base ? '**Benchmark harness or toolchain changed; timing deltas are not classified.**' : '**Initial benchmark baseline.**';
  const baseRef = payload.base ? `[${payload.base.commit.slice(0, 7)}](https://github.com/${repository}/commit/${payload.base.commit})` : 'none';
  return [marker, '## PBT benchmark results', '', summary, '',
    `Base: ${baseRef} · Head: [${headSha.slice(0, 7)}](https://github.com/${repository}/commit/${headSha}) · [CI run](${runUrl})`, '',
    'Base and head are measured on the same runner. Lower time is better. Changes are flagged only above 5% with non-overlapping 95% confidence intervals; flags are advisory.', '',
    '| Benchmark | Base mean | Head mean | Change |', '|---|---:|---:|---|', ...rows.slice(0, 100), '',
    ...(rows.length > 100 ? [`Showing 100 of ${rows.length} benchmarks; see the full artifact.`, ''] : []),
    ...(removed.length ? [`Removed benchmarks: ${removed.slice(0, 20).map(id => `\`${id}\``).join(', ')}${removed.length > 20 ? ' (additional removals in the artifact)' : ''}`, ''] : []),
    'Full estimates, confidence intervals, environment, and commit IDs are in the `benchmark-results` workflow artifact.',
  ].join('\n');
}
