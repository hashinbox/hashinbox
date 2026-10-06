import Box from '@mui/material/Box';
import ButtonBase from '@mui/material/ButtonBase';
import Stack from '@mui/material/Stack';
import Typography from '@mui/material/Typography';
import { useState } from 'react';
import { Prose } from '@/_components/prose';
import { mono } from '@/theme';
import { constraints, label, nested, refName, resolve, type Document, type Schema } from './spec';

interface Props {
  document: Document;
  schema: Schema;
  /** Component names already open above this one; a cycle stops unfolding. */
  seen?: string[];
}

/** A body's shape: its type, then one row per field, nested fields on demand. */
export function Shape({ document, schema, seen = [] }: Props) {
  const fields = nested(document, schema);
  return (
    <Box>
      <Typography sx={{ fontFamily: mono, fontSize: 12, color: 'text.secondary', mb: 1 }}>
        {label(schema)}
      </Typography>
      {fields ? (
        <Fields document={document} schema={fields} seen={seen} />
      ) : (
        <Detail document={document} schema={resolve(document, schema)} />
      )}
    </Box>
  );
}

function Fields({ document, schema, seen = [] }: Props) {
  const entries = Object.entries(schema.properties ?? {});
  if (entries.length === 0) {
    return <Typography sx={{ fontSize: 12.5, color: 'text.secondary' }}>No fields.</Typography>;
  }
  return (
    <Box sx={{ borderTop: 1, borderColor: 'divider' }}>
      {entries.map(([name, field]) => (
        <Field
          key={name}
          document={document}
          name={name}
          schema={field}
          required={schema.required?.includes(name) ?? false}
          seen={seen}
        />
      ))}
    </Box>
  );
}

interface FieldProps {
  document: Document;
  name: string;
  schema: Schema;
  required: boolean;
  seen: string[];
}

function Field({ document, name, schema, required, seen }: FieldProps) {
  const [open, setOpen] = useState(false);
  const resolved = resolve(document, schema);
  const ref = schema.$ref ?? resolved.items?.$ref;
  const cyclic = ref !== undefined && seen.includes(refName(ref));
  const inner = cyclic ? null : nested(document, schema);
  const count = Object.keys(inner?.properties ?? {}).length;
  return (
    <Box sx={{ py: 1.25, borderBottom: 1, borderColor: 'divider' }}>
      <Stack direction="row" spacing={1.25} sx={{ alignItems: 'baseline', flexWrap: 'wrap' }}>
        <Typography sx={{ fontFamily: mono, fontSize: 12.5, fontWeight: 500 }}>{name}</Typography>
        <Typography sx={{ fontFamily: mono, fontSize: 11.5, color: 'text.secondary' }}>
          {label(schema)}
        </Typography>
        <Typography
          sx={{
            fontFamily: mono,
            fontSize: 10,
            letterSpacing: '.1em',
            textTransform: 'uppercase',
            color: required ? 'warning.main' : 'text.disabled',
          }}
        >
          {required ? 'required' : 'optional'}
        </Typography>
      </Stack>
      <Detail document={document} schema={resolved} />
      {inner && count > 0 && (
        <>
          <ButtonBase
            onClick={() => setOpen(!open)}
            aria-expanded={open}
            sx={{
              mt: 1,
              fontFamily: mono,
              fontSize: 11,
              color: 'text.secondary',
              '&:hover': { color: 'text.primary' },
            }}
          >
            {open ? '− hide' : '+ show'} {count} {count === 1 ? 'field' : 'fields'}
          </ButtonBase>
          {open && (
            <Box sx={{ mt: 1, pl: 2, borderLeft: 1, borderColor: 'grey.400' }}>
              <Fields
                document={document}
                schema={inner}
                seen={ref ? [...seen, refName(ref)] : seen}
              />
            </Box>
          )}
        </>
      )}
    </Box>
  );
}

/** What a type alone does not say: its description, bounds and allowed values. */
function Detail({ document, schema }: { document: Document; schema: Schema }) {
  const bounds = constraints(schema);
  const values = schema.enum ?? resolve(document, schema.items ?? {}).enum;
  return (
    <>
      {schema.description && (
        <Box sx={{ mt: 0.5 }}>
          <Prose text={schema.description} size={12.5} />
        </Box>
      )}
      {(bounds.length > 0 || values) && (
        <Stack direction="row" sx={{ mt: 0.75, gap: 0.75, flexWrap: 'wrap' }}>
          {[...bounds, ...(values ?? []).map((v) => JSON.stringify(v))].map((text) => (
            <Box
              key={text}
              component="span"
              sx={{
                fontFamily: mono,
                fontSize: 10.5,
                px: 0.75,
                py: 0.125,
                border: 1,
                borderColor: 'divider',
                color: 'text.secondary',
              }}
            >
              {text}
            </Box>
          ))}
        </Stack>
      )}
    </>
  );
}
