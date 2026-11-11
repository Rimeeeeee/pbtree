import { readFileSync, appendFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { marker, renderReport } from './benchmark-report.mjs';

const event = JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, 'utf8'));
const run = event.workflow_run;
const repository = process.env.GITHUB_REPOSITORY;
if (run.name !== 'CI' || run.path !== '.github/workflows/ci.yml' || !['push', 'pull_request'].includes(run.event)) {
  throw new Error('Unexpected workflow run');
}
if (!/^[\w.-]+\/[\w.-]+$/.test(repository) || !/^[0-9a-f]{40}$/.test(run.head_sha)) throw new Error('Invalid run');
const prefix = `/repos/${repository}`;
async function api(path, method = 'GET', body) {
  const response = await fetch(`${process.env.GITHUB_API_URL}${path}`, {
    method,
    headers: { authorization: `Bearer ${process.env.GH_TOKEN}`, accept: 'application/vnd.github+json', 'content-type': 'application/json', 'X-GitHub-Api-Version': '2022-11-28' },
    ...(body ? { body: JSON.stringify(body) } : {}),
  });
  if (!response.ok) throw new Error(`GitHub API ${method} ${path}: ${response.status}`);
  return response;
}

const artifacts = await (await api(`${prefix}/actions/runs/${run.id}/artifacts?per_page=100`)).json();
const artifact = artifacts.artifacts.find(item => item.name === 'benchmark-results' && !item.expired);
let body;
if (artifact) {
  if (artifact.size_in_bytes > 5 * 1024 * 1024) throw new Error('Benchmark archive is too large');
  const download = await api(`${prefix}/actions/artifacts/${artifact.id}/zip`);
  const bytes = Buffer.from(await download.arrayBuffer());
  if (bytes.length > 5 * 1024 * 1024) throw new Error('Benchmark archive is too large');
  const json = execFileSync('python3', ['.github/scripts/read-benchmark-artifact.py'], { input: bytes, maxBuffer: 1024 * 1024 + 1 });
  body = renderReport(JSON.parse(json.toString('utf8')), { repository, headSha: run.head_sha, runUrl: run.html_url });
} else {
  body = `${marker}\n## PBT benchmark results\n\nNo complete benchmark results were produced. [Inspect the CI run](${run.html_url}) (conclusion: ${run.conclusion}).`;
}
appendFileSync(process.env.GITHUB_STEP_SUMMARY, `${body}\n`);

async function upsert(listPath, createPath, updatePath) {
  let existing;
  for (let page = 1; page <= 10; page++) {
    const comments = await (await api(`${listPath}?per_page=100&page=${page}`)).json();
    existing = comments.find(comment => comment.user?.login === 'github-actions[bot]' && comment.body?.includes(marker));
    if (existing || comments.length < 100) break;
  }
  await api(existing ? `${updatePath}/${existing.id}` : createPath, existing ? 'PATCH' : 'POST', { body });
}
if (run.event === 'pull_request') {
  // Resolve the PR through GitHub, never through the downloaded benchmark payload.
  const pulls = await (await api(`${prefix}/commits/${run.head_sha}/pulls?per_page=100`)).json();
  for (const pull of pulls.filter(pull => pull.head.sha === run.head_sha && pull.base.repo.full_name === repository && pull.state === 'open')) {
    const current = await (await api(`${prefix}/pulls/${pull.number}`)).json();
    if (current.head.sha !== run.head_sha) continue; // An older run must not replace a newer report.
    await upsert(`${prefix}/issues/${pull.number}/comments`, `${prefix}/issues/${pull.number}/comments`, `${prefix}/issues/comments`);
  }
} else if (run.head_branch === event.repository.default_branch) {
  await upsert(`${prefix}/commits/${run.head_sha}/comments`, `${prefix}/commits/${run.head_sha}/comments`, `${prefix}/comments`);
}
