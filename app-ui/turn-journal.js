// Keep uncertain submissions addressable by the same id after a tab reload.
export const JOURNAL_KEY = 'switchya.pendingTurns.v1';
export function readJournal(storage) {
  try {
    const parsed = JSON.parse(storage?.getItem(JOURNAL_KEY) || '{}');
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return {};
    return Object.fromEntries(Object.entries(parsed).filter(([id, turn]) =>
      turn && turn.session_id === id && typeof turn.command_id === 'string'
      && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(turn.command_id)
      && typeof turn.text === 'string' && turn.text.trim().length > 0));
  } catch { return {}; }
}
export function writeJournal(storage, journal) {
  try { storage?.setItem(JOURNAL_KEY, JSON.stringify(journal)); return Boolean(storage); }
  catch { return false; }
}
export function acknowledgedTurn(events, turn) {
  return Boolean(turn && events.some(event => event.session_id === turn.session_id
    && event.kind === 'turn.started' && event.payload?.command_id === turn.command_id));
}
