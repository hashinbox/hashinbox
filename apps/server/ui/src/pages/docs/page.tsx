import Box from '@mui/material/Box';
import ButtonBase from '@mui/material/ButtonBase';
import Stack from '@mui/material/Stack';
import Typography from '@mui/material/Typography';
import { useEffect, useMemo, useState } from 'react';
import { openapi } from '@/api/client';
import { Prose } from '@/_components/prose';
import type { Shell } from '@/layout';
import { mono } from '@/theme';
import { go, segments } from '@/route';
import { Operation } from './_components/operation';
import { Rail } from './_components/rail';
import { groups, isDocument, type Document, type Group } from './_components/spec';
import { Verb } from './_components/verb';
import { Split } from '@/_components/split';

/** The operation named after `docs/` in the address, so a reload or a shared link lands on it. */
function linked(): string | null {
  const [view, id] = segments();
  return view === 'docs' && id ? id : null;
}

interface Props {
  shell: Shell;
  /**
   * Whether this server asks for a token. The document marks every `/v1`
   * route as bearer-protected, because a token is required whenever the
   * server listens beyond loopback; a server that asked for none shows none.
   */
  secured: boolean;
}

/**
 * The API reference, rendered from the document the server itself serves at
 * `/openapi.json`: whatever this build of the server answers, this page shows.
 */
export function Docs({ shell, secured }: Props) {
  const [document, setDocument] = useState<Document | null>(null);
  const [failed, setFailed] = useState(false);
  const [selected, setSelected] = useState<string | null>(linked);

  useEffect(() => {
    let live = true;
    openapi()
      .then((value) => {
        if (!live) return;
        if (isDocument(value)) setDocument(value);
        else setFailed(true);
      })
      .catch(() => {
        if (live) setFailed(true);
      });
    return () => {
      live = false;
    };
  }, []);

  useEffect(() => {
    const follow = () => setSelected(linked());
    window.addEventListener('popstate', follow);
    return () => window.removeEventListener('popstate', follow);
  }, []);

  const all = useMemo(() => (document ? groups(document) : []), [document]);
  const endpoint = all.flatMap((g) => g.endpoints).find((e) => e.id === selected) ?? null;

  const select = (id: string | null) => {
    setSelected(id);
    go(id ? ['docs', id] : ['docs'], true);
    window.scrollTo({ top: 0 });
  };

  if (failed) {
    return (
      <Typography variant="h3" color="text.secondary" sx={{ px: { xs: 2, md: 4 }, py: 4 }}>
        The server did not answer with an OpenAPI 3.1 document.
      </Typography>
    );
  }
  if (!document) {
    return (
      <Typography
        color="text.secondary"
        sx={{ px: { xs: 2, md: 4 }, py: 4, fontFamily: mono, fontSize: 12 }}
      >
        reading /openapi.json…
      </Typography>
    );
  }
  return (
    <Split
      columns="minmax(260px, 320px) minmax(0, 1fr)"
      open={endpoint != null}
      onClose={() => select(null)}
      list={
        <Rail document={document} groups={all} selected={endpoint?.id ?? null} onSelect={select} />
      }
      detail={
        <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
          {endpoint ? (
            <Operation
              key={endpoint.id}
              document={document}
              endpoint={endpoint}
              secured={secured}
              onNotify={shell.notify}
            />
          ) : (
            <Overview document={document} groups={all} secured={secured} onSelect={select} />
          )}
        </Box>
      }
    />
  );
}

interface OverviewProps {
  document: Document;
  groups: Group[];
  secured: boolean;
  onSelect: (id: string) => void;
}

/** The document's own introduction, how it authenticates, and every tag at a glance. */
function Overview({ document, groups, secured, onSelect }: OverviewProps) {
  const schemes = secured ? Object.entries(document.components?.securitySchemes ?? {}) : [];
  return (
    <Box sx={{ maxWidth: 880 }}>
      <Typography variant="overline" component="p">
        OpenAPI {document.openapi} · v{document.info.version}
      </Typography>
      <Typography variant="h1" sx={{ mt: 1, mb: 2.5 }}>
        {document.info.title}
      </Typography>
      {document.info.description && <Prose text={document.info.description} size={15} />}

      {schemes.length > 0 && (
        <Box sx={{ mt: 4 }}>
          <Typography variant="overline" component="h2">
            Authentication
          </Typography>
          {schemes.map(([name, scheme]) => (
            <Box
              key={name}
              sx={{ mt: 1, py: 1.25, borderTop: 1, borderBottom: 1, borderColor: 'divider' }}
            >
              <Typography sx={{ fontFamily: mono, fontSize: 12.5 }}>
                {name} · {scheme.type}
                {scheme.scheme ? ` ${scheme.scheme}` : ''}
              </Typography>
              {scheme.description && (
                <Box sx={{ mt: 0.5 }}>
                  <Prose text={scheme.description} size={12.5} />
                </Box>
              )}
            </Box>
          ))}
        </Box>
      )}

      <Box
        sx={{
          mt: 5,
          display: 'grid',
          gap: 0,
          gridTemplateColumns: { xs: 'minmax(0, 1fr)', sm: 'repeat(2, minmax(0, 1fr))' },
          borderTop: 1,
          borderLeft: 1,
          borderColor: 'divider',
        }}
      >
        {groups.map((group) => (
          <Box
            key={group.tag}
            sx={{ p: 2.5, borderRight: 1, borderBottom: 1, borderColor: 'divider' }}
          >
            <Typography variant="h3" sx={{ textTransform: 'capitalize' }}>
              {group.tag}
            </Typography>
            {group.description && (
              <Typography color="text.secondary" sx={{ fontSize: 12.5, mt: 0.5, mb: 1.5 }}>
                {group.description}
              </Typography>
            )}
            <Stack spacing={0.5}>
              {group.endpoints.map((e) => (
                <ButtonBase
                  key={e.id}
                  onClick={() => onSelect(e.id)}
                  sx={{
                    justifyContent: 'flex-start',
                    gap: 1,
                    textAlign: 'left',
                    '&:hover .path': { textDecoration: 'underline' },
                  }}
                >
                  <Verb method={e.method} />
                  <Typography className="path" noWrap sx={{ fontFamily: mono, fontSize: 12 }}>
                    {e.path}
                  </Typography>
                </ButtonBase>
              ))}
            </Stack>
          </Box>
        ))}
      </Box>
    </Box>
  );
}
