import { execFileSync } from 'node:child_process';

const { packages } = JSON.parse(execFileSync('cargo', ['metadata', '--format-version', '1', '--no-deps'], { encoding: 'utf8' }));
const errors = [];
for (const pkg of packages) {
  for (const dependency of pkg.dependencies.filter(dependency => dependency.kind !== 'dev')) {
    const harness = dependency.name.startsWith('tickloop-agent-');
    if (pkg.name === 'tickloop-tick-installer' && harness) errors.push(`${pkg.name} depends on ${dependency.name}`);
    if (pkg.name === 'tickloop-tick-packages' && ['reqwest', 'hyper', 'tickloop-tick-installer'].includes(dependency.name)) errors.push(`${pkg.name} must keep installation networking separate`);
    if (['tickloop-agent-policy', 'tickloop-agent-runtime', 'tickloop-agent-test-support'].includes(pkg.name) && ['tickloop-agent-config', 'tickloop-agent-settings', 'tickloop-tick-installer'].includes(dependency.name)) errors.push(`${pkg.name} depends on Code settings or the installer: ${dependency.name}`);
    if (pkg.name === 'tickloop-agent-policy' && (dependency.name.startsWith('tickloop-tickhost') || ['tickloop-tick-packages', 'tickloop-ui'].includes(dependency.name))) errors.push(`policy depends on a local implementation: ${dependency.name}`);
  }
}
for (const obsolete of ['tickloop-agent-config', 'tickloop-agent-packages']) {
  if (packages.some(pkg => pkg.name === obsolete)) errors.push(`obsolete package remains: ${obsolete}`);
}
if (errors.length) throw new Error(errors.join('\n'));
console.log('Settings, policy and installer dependency boundaries passed.');
