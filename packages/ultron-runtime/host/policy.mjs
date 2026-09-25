export const CONTROL_NAMES = Object.freeze([
  'permissionPrompts', 'riskBlocking', 'capabilityEnforcement',
  'budgetEnforcement', 'completionGates', 'refinementApproval', 'sandboxRequired',
]);

export function resolveControls(overrides = {}) {
  if (!overrides || typeof overrides !== 'object' || Array.isArray(overrides)) throw new TypeError('controls must be an object');
  for (const [key, value] of Object.entries(overrides)) {
    if (!CONTROL_NAMES.includes(key) || typeof value !== 'boolean') throw new TypeError(`Invalid control: ${key}`);
  }
  return Object.freeze(Object.fromEntries(CONTROL_NAMES.map(key => [key, overrides[key] ?? false])));
}

export async function authorize(controls, request, services = {}) {
  if (controls.sandboxRequired && (!services.isolationVerified || await services.isolationVerified(request) !== true)) throw new Error('Sandbox required but unavailable');
  if (controls.capabilityEnforcement && (request.capabilities ?? []).some(c => !(request.grants ?? []).includes(c))) throw new Error('Capability not granted');
  if (controls.riskBlocking) {
    if (!services.screen || (await services.screen(request)) !== 'allow') throw new Error('Risk control refused execution');
  }
  if (controls.permissionPrompts) {
    if (!services.confirm || (await services.confirm(request)) !== true) throw new Error('Permission not approved');
  }
}
