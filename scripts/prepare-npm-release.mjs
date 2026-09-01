import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join } from 'node:path';

const repoRoot = process.cwd();
const sourcePackage = JSON.parse(readFileSync(join(repoRoot, 'package.json'), 'utf8'));
const buildRevision = process.env.NPM_BUILD_REVISION || '1';
const version = process.env.NPM_VERSION || `${sourcePackage.version}-custom.${buildRevision}`;
const scope = process.env.NPM_SCOPE || '@guanzhu.me';
const baseName = process.env.NPM_PACKAGE_BASENAME || 'wecom-cli';
const rootPackageName = `${scope}/${baseName}`;
const artifactRoot = join(repoRoot, '.release', 'artifacts');
const outputRoot = join(repoRoot, '.release', 'npm');

const platforms = [
  { id: 'darwin-arm64', os: 'darwin', cpu: 'arm64', binary: 'wecom-cli' },
  { id: 'darwin-x64', os: 'darwin', cpu: 'x64', binary: 'wecom-cli' },
  { id: 'linux-arm64', os: 'linux', cpu: 'arm64', binary: 'wecom-cli' },
  { id: 'linux-x64', os: 'linux', cpu: 'x64', binary: 'wecom-cli' },
  { id: 'win32-x64', os: 'win32', cpu: 'x64', binary: 'wecom-cli.exe' },
];

function writeJson(path, value) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);
}

function ensureFile(path, description) {
  if (!existsSync(path)) {
    throw new Error(`Missing ${description}: ${path}`);
  }
}

rmSync(outputRoot, { recursive: true, force: true });
mkdirSync(outputRoot, { recursive: true });

const optionalDependencies = {};

for (const platform of platforms) {
  const packageName = `${rootPackageName}-${platform.id}`;
  optionalDependencies[packageName] = version;

  const sourceBinary = join(artifactRoot, `binary-${platform.id}`, platform.binary);
  ensureFile(sourceBinary, `${platform.id} build artifact`);

  const packageDir = join(outputRoot, platform.id);
  const targetBinary = join(packageDir, 'bin', platform.binary);
  mkdirSync(dirname(targetBinary), { recursive: true });
  copyFileSync(sourceBinary, targetBinary);

  if (platform.os !== 'win32') {
    chmodSync(targetBinary, 0o755);
  }

  writeJson(join(packageDir, 'package.json'), {
    name: packageName,
    version,
    description: `${platform.id} binary for ${rootPackageName} (custom-endpoint enabled)`,
    license: 'MIT',
    files: ['bin', 'README.md', 'LICENSE'],
    engines: { node: '>=18' },
    os: [platform.os],
    cpu: [platform.cpu],
    publishConfig: { access: 'public' },
  });

  copyFileSync(join(repoRoot, 'LICENSE'), join(packageDir, 'LICENSE'));
  writeFileSync(
    join(packageDir, 'README.md'),
    `# ${packageName}\n\nPlatform binary package for \`${rootPackageName}\`.\n\nThis binary is built from \`wn0x00/wecom-cli\` with the Rust \`custom-endpoint\` feature enabled.\n`,
  );
}

const launcherSource = readFileSync(join(repoRoot, 'bin', 'wecom.js'), 'utf8');
let launcher = launcherSource;

for (const platform of platforms) {
  launcher = launcher.replaceAll(
    `@wecom/cli-${platform.id}`,
    `${rootPackageName}-${platform.id}`,
  );
}

launcher = launcher.replaceAll('@wecom/cli', rootPackageName);

const rootDir = join(outputRoot, 'root');
mkdirSync(join(rootDir, 'bin'), { recursive: true });
writeFileSync(join(rootDir, 'bin', 'wecom.js'), launcher, { mode: 0o755 });

writeJson(join(rootDir, 'package.json'), {
  name: rootPackageName,
  version,
  description: 'WeCom CLI build with custom-endpoint support enabled',
  keywords: ['wecom-cli', 'wecom', 'custom-endpoint'],
  homepage: 'https://github.com/wn0x00/wecom-cli#readme',
  bugs: { url: 'https://github.com/wn0x00/wecom-cli/issues' },
  repository: { type: 'git', url: 'https://github.com/wn0x00/wecom-cli.git' },
  license: 'MIT',
  type: 'module',
  bin: { 'wecom-cli': 'bin/wecom.js' },
  files: ['bin', 'README.md', 'LICENSE'],
  optionalDependencies,
  publishConfig: { access: 'public' },
  engines: { node: '>=18' },
});

copyFileSync(join(repoRoot, 'LICENSE'), join(rootDir, 'LICENSE'));

const upstreamReadme = readFileSync(join(repoRoot, 'README.md'), 'utf8');
const releaseNotice = `# ${rootPackageName}\n\n> Unofficial npm build of [WecomTeam/wecom-cli](https://github.com/WecomTeam/wecom-cli) from the \`wn0x00/wecom-cli\` fork. The published native binaries are compiled with \`--features custom-endpoint\`, enabling \`WECOM_CLI_BASE_URL\`, \`WECOM_CLI_AUTH_ENDPOINT\`, and \`WECOM_CLI_ACCESS_TOKEN\`.\n\nInstall:\n\n\`\`\`bash\nnpm install -g ${rootPackageName}\n\`\`\`\n\nExample:\n\n\`\`\`bash\nexport WECOM_CLI_BASE_URL=https://your-endpoint.example.com\nwecom-cli --help\n\`\`\`\n\n---\n\n`;
writeFileSync(join(rootDir, 'README.md'), releaseNotice + upstreamReadme);

console.log(`Prepared ${rootPackageName}@${version}`);
console.log(`Output: ${outputRoot}`);
