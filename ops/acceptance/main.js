import * as auth from './auth.js';
auth.configureAuth({baseUrl: location.origin});
const field = id => document.getElementById(id);
// Remove a pasted email URL from browser history immediately; no analytics.
const token = new URL(location.href).searchParams.get('token');
if (token) field('token').value = token;
history.replaceState(null, '', location.pathname);
field('run').onclick = async () => {
  const operation = field('operation').value;
  if (['deleteAccount', 'deletePasskey', 'logoutEverywhere', 'revokeOtherSessions', 'revokeSession'].includes(operation) && !confirm(`Run ${operation} on this test account?`)) return;
  const args = {
    requestSignup: [field('email').value], requestRecovery: [field('email').value],
    requestEmailChange: [field('email').value],
    finishSignup: [field('token').value, field('name').value],
    finishRecovery: [field('token').value, field('name').value],
    confirmEmailChange: [field('token').value], registerPasskey: [field('name').value],
    deletePasskey: [field('id').value], revokeSession: [field('id').value],
  };
  field('run').disabled = true;
  try {
    const value = await auth[operation](...(args[operation] || []));
    field('result').textContent = `${operation}: passed at ${new Date().toISOString()}`;
    if (operation === 'listPasskeys') field('result').textContent += '\n' + value.map(p => `${p.name}: ${p.credential_id}`).join('\n');
    if (operation === 'listSessions') field('result').textContent += '\n' + value.map(s => `${s.is_current ? 'current' : 'other'}: ${s.id}`).join('\n');
  } catch (error) {
    field('result').textContent = `${operation}: failed (${error.status || 'browser'} / ${error.code || error.name})`;
  } finally { field('token').value = ''; field('run').disabled = false; }
};
