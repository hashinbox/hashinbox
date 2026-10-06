import Box from '@mui/material/Box';
import ButtonBase from '@mui/material/ButtonBase';
import Stack from '@mui/material/Stack';
import Typography from '@mui/material/Typography';
import { useState, type ReactNode } from 'react';
import { Prose } from '@/_components/prose';
import { mono } from '@/theme';
import { Console } from './console';
import { Shape } from './schema';
import { constraints, guarded, label, type Document, type Endpoint, type Response } from './spec';
import { Verb } from './verb';

interface Props {
  document: Document;
  endpoint: Endpoint;
  /** Whether this server asks for a token at all. */
  secured: boolean;
  onNotify: (text: string, bad?: boolean) => void;
}

/** One operation: what it does, what it takes, what it answers, and a console. */
export function Operation({ document, endpoint, secured, onNotify }: Props) {
  const needsToken = secured && guarded(endpoint.operation);
  const { operation } = endpoint;
  const parameters = operation.parameters ?? [];
  const body = operation.requestBody?.content?.['application/json']?.schema;
  return (
    <Box>
      <Stack direction="row" spacing={1.5} sx={{ alignItems: 'baseline', flexWrap: 'wrap' }}>
        <Verb method={endpoint.method} wide />
        <Typography sx={{ fontFamily: mono, fontSize: 15, wordBreak: 'break-all' }}>
          {endpoint.path}
        </Typography>
        {needsToken && (
          <Typography
            sx={{
              fontFamily: mono,
              fontSize: 10,
              letterSpacing: '.12em',
              textTransform: 'uppercase',
              color: 'text.secondary',
              border: 1,
              borderColor: 'divider',
              px: 0.75,
            }}
          >
            bearer token
          </Typography>
        )}
      </Stack>
      <Typography variant="h2" component="h1" sx={{ mt: 1.5, mb: 1.5 }}>
        {operation.summary ?? endpoint.id}
      </Typography>
      {operation.description && <Prose text={operation.description} />}

      <Box
        sx={{
          mt: 4,
          display: 'grid',
          gap: { xs: 4, xl: 5 },
          gridTemplateColumns: { xs: 'minmax(0, 1fr)', xl: 'minmax(0, 1fr) minmax(0, 1fr)' },
          alignItems: 'start',
        }}
      >
        <Stack spacing={4} sx={{ minWidth: 0 }}>
          {parameters.length > 0 && (
            <Section title="Parameters">
              <Box sx={{ borderTop: 1, borderColor: 'divider' }}>
                {parameters.map((p) => (
                  <Box
                    key={`${p.in}-${p.name}`}
                    sx={{ py: 1.25, borderBottom: 1, borderColor: 'divider' }}
                  >
                    <Stack
                      direction="row"
                      spacing={1.25}
                      sx={{ alignItems: 'baseline', flexWrap: 'wrap' }}
                    >
                      <Typography sx={{ fontFamily: mono, fontSize: 12.5, fontWeight: 500 }}>
                        {p.name}
                      </Typography>
                      <Typography
                        sx={{ fontFamily: mono, fontSize: 11.5, color: 'text.secondary' }}
                      >
                        {p.schema ? label(p.schema) : 'string'} · {p.in}
                      </Typography>
                      {p.required && (
                        <Typography
                          sx={{
                            fontFamily: mono,
                            fontSize: 10,
                            letterSpacing: '.1em',
                            textTransform: 'uppercase',
                            color: 'warning.main',
                          }}
                        >
                          required
                        </Typography>
                      )}
                      {p.schema &&
                        constraints(p.schema).map((c) => (
                          <Typography
                            key={c}
                            sx={{ fontFamily: mono, fontSize: 10.5, color: 'text.secondary' }}
                          >
                            {c}
                          </Typography>
                        ))}
                    </Stack>
                    {p.description && (
                      <Box sx={{ mt: 0.5 }}>
                        <Prose text={p.description} size={12.5} />
                      </Box>
                    )}
                  </Box>
                ))}
              </Box>
            </Section>
          )}
          {body && (
            <Section title="Request body">
              <Shape document={document} schema={body} />
            </Section>
          )}
          <Section title="Responses">
            <Box sx={{ borderTop: 1, borderColor: 'divider' }}>
              {Object.entries(operation.responses).map(([status, response]) => (
                <Answer key={status} document={document} status={status} response={response} />
              ))}
            </Box>
          </Section>
        </Stack>
        <Box sx={{ minWidth: 0, position: { xl: 'sticky' }, top: { xl: 84 } }}>
          <Console
            key={endpoint.id}
            document={document}
            endpoint={endpoint}
            secured={needsToken}
            onNotify={onNotify}
          />
        </Box>
      </Box>
    </Box>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <Box component="section">
      <Typography variant="overline" component="h2" sx={{ display: 'block', mb: 1 }}>
        {title}
      </Typography>
      {children}
    </Box>
  );
}

/** One status the operation may answer with; its body unfolds on demand. */
function Answer({
  document,
  status,
  response,
}: {
  document: Document;
  status: string;
  response: Response;
}) {
  const [open, setOpen] = useState(false);
  const schema = response.content?.['application/json']?.schema;
  const tone = status.startsWith('2')
    ? 'success.main'
    : status.startsWith('5')
      ? 'error.main'
      : 'warning.main';
  return (
    <Box sx={{ borderBottom: 1, borderColor: 'divider' }}>
      <ButtonBase
        onClick={() => setOpen(!open)}
        disabled={!schema}
        aria-expanded={open}
        sx={{
          width: '100%',
          justifyContent: 'flex-start',
          textAlign: 'left',
          gap: 1.5,
          py: 1.25,
          '&:hover .label': { color: 'text.primary' },
        }}
      >
        <Typography
          sx={{
            fontFamily: mono,
            fontSize: 12.5,
            fontWeight: 500,
            color: tone,
            width: 32,
            flex: 'none',
          }}
        >
          {status}
        </Typography>
        <Typography sx={{ fontSize: 13, flexGrow: 1 }}>{response.description}</Typography>
        {schema && (
          <Typography
            className="label"
            sx={{ fontFamily: mono, fontSize: 11.5, color: 'text.secondary' }}
          >
            {label(schema)} {open ? '−' : '+'}
          </Typography>
        )}
      </ButtonBase>
      {open && schema && (
        <Box sx={{ pb: 2, pl: 5.5 }}>
          <Shape document={document} schema={schema} />
        </Box>
      )}
    </Box>
  );
}
