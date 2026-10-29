import type {
  ActionItem,
  ActionRow,
  Citation,
  Config,
  ConfigPatch,
  Hit,
  ListFilter,
  Meeting,
  MeetingDetail,
  MeetingPatch,
  MeetingRow,
  Person,
  RecallEvent,
  SelfTest,
  Status,
  Step,
} from './types';

const BASE = '/api';

/** `GET /api/meetings` URL with only the filter fields that are set. */
export function listUrl(filter: ListFilter): string {
  const params = new URLSearchParams();
  for (const key of ['q', 'app', 'participant', 'from', 'to'] as const) {
    const v = filter[key];
    if (v) params.set(key, v);
  }
  const qs = params.toString();
  return qs ? `${BASE}/meetings?${qs}` : `${BASE}/meetings`;
}

/** Meeting ids are directory names with spaces and colons, so encode them. */
export function meetingUrl(id: string): string {
  return `${BASE}/meetings/${encodeURIComponent(id)}`;
}

export function audioUrl(id: string): string {
  return `${meetingUrl(id)}/audio`;
}

export function screenUrl(id: string): string {
  return `${meetingUrl(id)}/screen`;
}

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

/** The server sends failures as `{ "error": message }`; other bodies pass through. */
function errorMessage(text: string): string {
  try {
    const body = JSON.parse(text) as unknown;
    if (body && typeof body === 'object' && typeof (body as { error?: unknown }).error === 'string') {
      return (body as { error: string }).error;
    }
  } catch {
    // Not JSON, e.g. a proxy's error page.
  }
  return text;
}

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init);
  if (!res.ok) {
    const text = await res.text().catch(() => '');
    throw new ApiError(res.status, errorMessage(text) || res.statusText);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

function json(method: string, body: unknown): RequestInit {
  return {
    method,
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  };
}

/** Lists meetings; `hits` holds the transcript matches when `q` is set. */
export async function listMeetings(
  filter: ListFilter,
): Promise<{ meetings: MeetingRow[]; hits: Hit[] }> {
  const body = await request<{ meetings: MeetingRow[]; hits?: Hit[] }>(listUrl(filter));
  return { meetings: body.meetings, hits: body.hits ?? [] };
}

export function getMeeting(id: string): Promise<MeetingDetail> {
  return request(meetingUrl(id));
}

export function patchMeeting(id: string, body: MeetingPatch): Promise<Meeting> {
  return request(meetingUrl(id), json('PATCH', body));
}

export function deleteMeeting(id: string): Promise<void> {
  return request(meetingUrl(id), { method: 'DELETE' });
}

export function rerun(id: string, step: Step): Promise<{ job: number }> {
  return request(`${meetingUrl(id)}/rerun/${step}`, { method: 'POST' });
}

/** Every meeting's action items, newest meeting first. */
export function actionItems(): Promise<ActionRow[]> {
  return request(`${BASE}/action-items`);
}

/**
 * Checks an action item off or reopens it. `text` is the item as last
 * seen; the server answers 409 when a rerun has replaced the items since.
 */
export function patchAction(
  id: string,
  index: number,
  body: { done: boolean; text?: string },
): Promise<ActionItem> {
  return request(`${meetingUrl(id)}/actions/${index}`, json('PATCH', body));
}

export function people(): Promise<Person[]> {
  return request(`${BASE}/people`);
}

export function patchPerson(
  id: string,
  body: { name?: string; merge_into?: string },
): Promise<unknown> {
  return request(`${BASE}/people/${encodeURIComponent(id)}`, json('PATCH', body));
}

export function getConfig(): Promise<Config> {
  return request(`${BASE}/config`);
}

/** Merged into the config file; the daemon applies it after a restart. */
export function putConfig(patch: ConfigPatch): Promise<{ restart_required: boolean }> {
  return request(`${BASE}/config`, json('PUT', patch));
}

/** Records a few seconds from both sources; resolves when it is done. */
export function selftest(): Promise<SelfTest> {
  return request(`${BASE}/selftest`, { method: 'POST' });
}

export function status(): Promise<Status> {
  return request(`${BASE}/status`);
}

/** `POST /api/record/{start|stop|discard|screen_off}`; the daemon answers 204. */
export function record(action: 'start' | 'stop' | 'discard' | 'screen_off'): Promise<void> {
  return request(`${BASE}/record/${action}`, { method: 'POST' });
}

export interface SseMessage {
  event: string;
  data: string;
}

/**
 * Incremental parser for a `text/event-stream` body. `push` takes the next
 * decoded chunk and returns the messages it completed.
 */
export function parseSse() {
  let buf = '';
  return {
    push(chunk: string): SseMessage[] {
      buf += chunk.replace(/\r\n?/g, '\n');
      const out: SseMessage[] = [];
      let end: number;
      while ((end = buf.indexOf('\n\n')) !== -1) {
        const block = buf.slice(0, end);
        buf = buf.slice(end + 2);
        let event = 'message';
        const data: string[] = [];
        for (const line of block.split('\n')) {
          if (line.startsWith(':')) continue;
          const colon = line.indexOf(':');
          const field = colon === -1 ? line : line.slice(0, colon);
          let value = colon === -1 ? '' : line.slice(colon + 1);
          if (value.startsWith(' ')) value = value.slice(1);
          if (field === 'event') event = value;
          else if (field === 'data') data.push(value);
        }
        if (data.length > 0 || event !== 'message') out.push({ event, data: data.join('\n') });
      }
      return out;
    },
  };
}

/**
 * Asks a question about a meeting and streams the answer. `EventSource`
 * cannot POST, so the SSE body is read from `fetch` directly. Resolves when
 * the server sends `event: done` or closes the stream; rejects with an
 * `ApiError` carrying the message when it sends `event: error` (the model
 * failed mid-answer).
 */
export async function ask(
  id: string,
  q: string,
  onToken: (token: string) => void,
  signal?: AbortSignal,
): Promise<void> {
  const res = await fetch(`${meetingUrl(id)}/ask`, { ...json('POST', { question: q }), signal });
  if (!res.ok || !res.body) {
    throw new ApiError(res.status, errorMessage(await res.text().catch(() => res.statusText)));
  }
  const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
  const parser = parseSse();
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      for (const msg of parser.push(value)) {
        if (msg.event === 'done') return;
        if (msg.event === 'error') throw new ApiError(0, msg.data);
        if (msg.event === 'message') onToken(msg.data);
      }
    }
  } finally {
    reader.cancel().catch(() => {});
  }
}

/** The server's text for a cross-meeting question while another runs (409). */
export const ASK_ALL_BUSY = 'another question is being answered; try again when it is done';

export interface AskAllHandlers {
  /** A tool call: `search_meetings`, `get_transcript`, `Read`, ... */
  onTool: (name: string, detail: string | null) => void;
  /** A piece of the model's text; text before a tool call is not the answer. */
  onText: (text: string) => void;
}

/**
 * Asks Claude Code a question across the synced meetings (`POST
 * /api/ask`), reporting progress through `handlers`. Resolves with the
 * final answer and the meetings it cites; rejects with an `ApiError` for a
 * refused request or an `event: error`.
 */
export async function askAll(
  q: string,
  handlers: AskAllHandlers,
  signal?: AbortSignal,
): Promise<{ text: string; citations: Citation[] }> {
  const res = await fetch(`${BASE}/ask`, { ...json('POST', { question: q }), signal });
  if (!res.ok || !res.body) {
    throw new ApiError(res.status, errorMessage(await res.text().catch(() => res.statusText)));
  }
  const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
  const parser = parseSse();
  let answer: { text: string; citations: Citation[] } | null = null;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      for (const msg of parser.push(value)) {
        if (msg.event === 'error') throw new ApiError(0, msg.data);
        if (msg.event === 'tool') {
          const t = JSON.parse(msg.data) as { name: string; detail: string | null };
          handlers.onTool(t.name, t.detail);
        } else if (msg.event === 'delta') {
          handlers.onText(JSON.parse(msg.data) as string);
        } else if (msg.event === 'answer') {
          answer = JSON.parse(msg.data) as { text: string; citations: Citation[] };
        } else if (msg.event === 'done') {
          if (answer) return answer;
          throw new ApiError(0, 'no answer');
        }
      }
    }
  } finally {
    reader.cancel().catch(() => {});
  }
  if (answer) return answer;
  throw new ApiError(0, 'the answer stream ended early');
}

/** Subscribes to `/api/events`. Returns a function that closes the stream. */
export function events(onEvent: (e: RecallEvent) => void): () => void {
  const source = new EventSource(`${BASE}/events`);
  source.onmessage = (m) => {
    try {
      onEvent(JSON.parse(m.data) as RecallEvent);
    } catch {
      // A malformed event is skipped; the stream itself stays open.
    }
  };
  return () => source.close();
}
