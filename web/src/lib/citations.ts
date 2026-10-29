// Citations in a cross-meeting answer. Claude Code writes them as
// `[[<meeting id> @ <mm:ss or h:mm:ss>]]`, the format the server's prompt
// asks for and its own parser (crates/recall-server/src/qa.rs) reads.

export type Piece =
  | { kind: 'text'; text: string }
  | { kind: 'cite'; id: string; ms: number; time: string; raw: string };

const CITATION = /\[\[([^[\]]+?)\s*@\s*(\d{1,2}(?::\d{2}){1,2})\s*\]\]/g;

/** `mm:ss` or `h:mm:ss` in milliseconds; `null` when it is not one. */
export function parseTime(time: string): number | null {
  const parts = time.split(':');
  if (parts.length < 2 || parts.length > 3) return null;
  let seconds = 0;
  for (const [i, p] of parts.entries()) {
    if (!/^\d{1,2}$/.test(p)) return null;
    const n = Number(p);
    if (i > 0 && n >= 60) return null;
    seconds = seconds * 60 + n;
  }
  return seconds * 1000;
}

/**
 * Splits `text` into plain text and citations of the meetings in `known`
 * (the ones the server confirmed exist and are synced). A citation of any
 * other id stays plain text.
 */
export function splitCitations(text: string, known: ReadonlySet<string>): Piece[] {
  const out: Piece[] = [];
  let last = 0;
  const push = (t: string) => {
    if (!t) return;
    const prev = out[out.length - 1];
    if (prev?.kind === 'text') prev.text += t;
    else out.push({ kind: 'text', text: t });
  };
  for (const m of text.matchAll(CITATION)) {
    const id = m[1].trim();
    const ms = parseTime(m[2]);
    push(text.slice(last, m.index));
    if (ms !== null && known.has(id)) {
      out.push({ kind: 'cite', id, ms, time: m[2], raw: m[0] });
    } else {
      push(m[0]);
    }
    last = m.index + m[0].length;
  }
  push(text.slice(last));
  return out;
}
