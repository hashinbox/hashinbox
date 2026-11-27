import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState, type SubmitEvent } from 'react';
import { ApiError, address, exchange, type Exchange } from '@/api/client';
import { Copy } from '@/_components/copy';
import { mono } from '@/theme';
import { example, type Document, type Endpoint } from './spec';

interface Props {
  document: Document;
  endpoint: Endpoint;
  /** Whether this operation, on this server, takes the tab's token. */
  secured: boolean;
  onNotify: (text: string, bad?: boolean) => void;
}

/** The request as typed: one value per parameter, and the body text. */
interface Draft {
  values: Record<string, string>;
  body: string;
}

function initial(document: Document, endpoint: Endpoint): Draft {
  const schema = endpoint.operation.requestBody?.content?.['application/json']?.schema;
  return {
    values: {},
    body: schema ? JSON.stringify(example(document, schema), null, 2) : '',
  };
}

/** The path with its parameters filled in, and the query string of the set ones. */
function target(endpoint: Endpoint, values: Record<string, string>): string {
  const parameters = endpoint.operation.parameters ?? [];
  const path = endpoint.path.replace(/\{([^}]+)\}/g, (whole, name: string) => {
    const value = values[name];
    return value ? encodeURIComponent(value) : whole;
  });
  const query = new URLSearchParams();
  for (const p of parameters.filter((p) => p.in === 'query')) {
    const value = values[p.name];
    if (value) query.set(p.name, value);
  }
  const text = query.toString();
  return text ? `${path}?${text}` : path;
}

function curl(
  endpoint: Endpoint,
  path: string,
  body: string | undefined,
  secured: boolean,
): string {
  const lines = [`curl -X ${endpoint.method.toUpperCase()} '${address(path)}'`];
  if (secured) lines.push(`  -H "Authorization: Bearer $TOKEN"`);
  if (body !== undefined) {
    lines.push(`  -H 'Content-Type: application/json'`);
    lines.push(`  -d '${body.replace(/'/g, "'\\''")}'`);
  }
  return lines.join(' \\\n');
}

function pretty(exchanged: Exchange): string {
  if (!exchanged.contentType?.includes('json')) return exchanged.body;
  try {
    return JSON.stringify(JSON.parse(exchanged.body), null, 2);
  } catch {
    return exchanged.body;
  }
}

/** Sends the operation against this server and shows exactly what came back. */
export function Console({ document, endpoint, secured, onNotify }: Props) {
  const [draft, setDraft] = useState(() => initial(document, endpoint));
  const [answer, setAnswer] = useState<Exchange | null>(null);
  const [busy, setBusy] = useState(false);

  const parameters = (endpoint.operation.parameters ?? []).filter(
    (p) => p.in === 'path' || p.in === 'query',
  );
  const hasBody = endpoint.operation.requestBody !== undefined;
  const path = target(endpoint, draft.values);
  const missing = parameters.some((p) => p.in === 'path' && !draft.values[p.name]);
  const body = hasBody ? draft.body : undefined;

  const send = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (body !== undefined) {
      try {
        JSON.parse(body);
      } catch {
        onNotify('The body is not valid JSON.', true);
        return;
      }
    }
    setBusy(true);
    exchange(endpoint.method, path, body)
      .then(setAnswer)
      .catch((error: unknown) => {
        onNotify(ApiError.from(error).message, true);
      })
      .finally(() => setBusy(false));
  };

  const ok = answer !== null && answer.status < 400;
  return (
    <Box component="form" onSubmit={send} sx={{ border: 1, borderColor: 'grey.400' }}>
      <Stack
        direction="row"
        spacing={1.5}
        sx={{ alignItems: 'center', px: 2, py: 1.25, borderBottom: 1, borderColor: 'divider' }}
      >
        <Typography variant="overline" sx={{ flexGrow: 1 }}>
          Try it
        </Typography>
        {secured && (
          <Typography sx={{ fontFamily: mono, fontSize: 11, color: 'text.secondary' }}>
            sends this tab’s token
          </Typography>
        )}
        <Button type="submit" variant="contained" disabled={busy || missing}>
          {busy ? 'Sending…' : 'Send'}
        </Button>
      </Stack>

      <Stack spacing={2} sx={{ px: 2, py: 2 }}>
        {parameters.map((p) => (
          <TextField
            key={`${p.in}-${p.name}`}
            label={`${p.name} · ${p.in}${p.required ? '' : ' · optional'}`}
            value={draft.values[p.name] ?? ''}
            onChange={(e) => {
              setDraft({ ...draft, values: { ...draft.values, [p.name]: e.target.value } });
            }}
            slotProps={{ htmlInput: { spellCheck: false } }}
          />
        ))}
        {hasBody && (
          <TextField
            label="body · application/json"
            value={draft.body}
            onChange={(e) => setDraft({ ...draft, body: e.target.value })}
            multiline
            minRows={4}
            slotProps={{ htmlInput: { spellCheck: false, style: { fontSize: 12.5 } } }}
          />
        )}
        <Box>
          <Stack direction="row" sx={{ alignItems: 'baseline', mb: 0.75 }}>
            <Typography variant="overline" sx={{ flexGrow: 1 }}>
              curl
            </Typography>
            <Copy text={curl(endpoint, path, body, secured)} label="copy" onDone={onNotify} />
          </Stack>
          <Code text={curl(endpoint, path, body, secured)} />
        </Box>
      </Stack>

      {answer && (
        <Box sx={{ borderTop: 1, borderColor: 'divider', px: 2, py: 2 }}>
          <Stack direction="row" spacing={1.5} sx={{ alignItems: 'baseline', mb: 1 }}>
            <Typography
              sx={{
                fontFamily: mono,
                fontSize: 13,
                fontWeight: 500,
                color: ok ? 'success.main' : 'error.main',
              }}
            >
              {answer.status} {answer.statusText}
            </Typography>
            <Typography sx={{ fontFamily: mono, fontSize: 11.5, color: 'text.secondary' }}>
              {answer.millis} ms
            </Typography>
          </Stack>
          <Code text={pretty(answer) || '(empty body)'} />
        </Box>
      )}
    </Box>
  );
}

/** Exact text: mono, preserved, scrolled rather than wrapped. */
export function Code({ text }: { text: string }) {
  return (
    <Box
      component="pre"
      sx={{
        m: 0,
        p: 1.5,
        fontFamily: mono,
        fontSize: 12,
        lineHeight: 1.6,
        overflow: 'auto',
        maxHeight: 420,
        bgcolor: 'action.hover',
        border: 1,
        borderColor: 'divider',
      }}
    >
      {text}
    </Box>
  );
}
