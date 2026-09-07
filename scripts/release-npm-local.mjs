import { copyFileSync, existsSync, mkdirSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { join } from 'node:path';

const action = process.argv[2];
if (!['prepare', 'publish'].includes(action)) {
  throw new Error('Usage: node scripts/release-npm-local.mjs <prepare|publish>');
}

const repoRoot = process.cwd();
const registry = 'https://registry.npmjs.org/';
const scope = process.env.NPM_SCOPE || '@xiaojian-cli';
const platforms = 'darwin-arm64,linux-x64';
const artifacts = join(repoRoot, '.release', 'artifacts');

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { cwd: repoRoot, stdio: 'inherit', ...options });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} ${args.join(' ')} failed`);
}

function stage(platform, source) {
  if (!existsSync(source)) throw new Error(`Missing ${platform} binary: ${source}`);
  const target = join(artifacts, `binary-${platform}`, 'wecom-cli');
  mkdirSync(join(artifacts, `binary-${platform}`), { recursive: true });
  copyFileSync(source, target);
}

if (action === 'publish') {
  run('npm', ['whoami', `--registry=${registry}`]);
}

run('cargo', [
  'build', '--locked', '--release', '-p', 'wecom-cli',
  '--target', 'aarch64-apple-darwin', '--features', 'custom-endpoint',
]);
stage('darwin-arm64', join(repoRoot, 'target', 'aarch64-apple-darwin', 'release', 'wecom-cli'));

run('docker', [
  'run', '--rm', '--platform=linux/amd64',
  '-v', `${repoRoot}:/workspace`, '-w', '/workspace',
  'rust:1.88-bookworm',
  'bash', '-c',
  'CARGO_TARGET_DIR=/workspace/.release/linux-target cargo build --locked --release -p wecom-cli --features custom-endpoint',
]);
stage('linux-x64', join(repoRoot, '.release', 'linux-target', 'release', 'wecom-cli'));

run('node', ['scripts/prepare-npm-release.mjs'], {
  env: { ...process.env, NPM_SCOPE: scope, NPM_PLATFORMS: platforms },
});

for (const dir of ['.release/npm/darwin-arm64', '.release/npm/linux-x64', '.release/npm/root']) {
  run('npm', ['pack', dir, '--dry-run', `--registry=${registry}`]);
}

if (action === 'publish') {
  for (const dir of ['.release/npm/darwin-arm64', '.release/npm/linux-x64', '.release/npm/root']) {
    run('npm', ['publish', dir, '--access', 'public', '--tag', 'latest', `--registry=${registry}`]);
  }
}
