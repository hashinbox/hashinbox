import Add from '@mui/icons-material/Add';
import Close from '@mui/icons-material/Close';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import IconButton from '@mui/material/IconButton';
import MenuItem from '@mui/material/MenuItem';
import TextField from '@mui/material/TextField';
import Tooltip from '@mui/material/Tooltip';
import Typography from '@mui/material/Typography';
import type { ReactNode } from 'react';
import { nodeOf, rekind, type Logic } from './forms';

/** Fields and words on one line, wrapping; a block inside starts its own lines. */
export function Sentence({ children }: { children: ReactNode }) {
  return (
    <Box sx={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: 1, minWidth: 0 }}>
      {children}
    </Box>
  );
}

/** A plain word between fields. */
export function Word({ children }: { children: ReactNode }) {
  return (
    <Typography component="span" variant="body2" sx={{ whiteSpace: 'nowrap' }}>
      {children}
    </Typography>
  );
}

/** A sentence that may be removed: the × sits on its first line. */
export function Row({
  children,
  onRemove,
}: {
  children: ReactNode;
  onRemove?: (() => void) | undefined;
}) {
  return (
    <Box
      sx={{
        display: 'grid',
        gridTemplateColumns: 'minmax(0, max-content) auto',
        alignItems: 'start',
        columnGap: 0.5,
      }}
    >
      <Sentence>{children}</Sentence>
      {onRemove && (
        <Box sx={{ height: 40, display: 'flex', alignItems: 'center' }}>
          <Tooltip title="Remove">
            <IconButton size="small" aria-label="Remove" onClick={onRemove}>
              <Close fontSize="small" />
            </IconButton>
          </Tooltip>
        </Box>
      )}
    </Box>
  );
}

/** What a part holds, indented under a thin rule, on lines of its own. */
export function Block({ children }: { children: ReactNode }) {
  return (
    <Box
      sx={{
        flexBasis: '100%',
        ml: 0.5,
        pl: 1.5,
        py: 0.25,
        borderLeft: 1,
        borderColor: 'divider',
        display: 'grid',
        gap: 1,
      }}
    >
      {children}
    </Box>
  );
}

/** A select as wide as its words; `label` names it for screen readers only. */
export function Choice({
  label,
  value,
  options,
  onChange,
}: {
  label: string;
  value: string;
  /** `menu`: how the option reads in the open menu, when not as `label`. */
  options: { value: string; label: string; menu?: string }[];
  onChange: (value: string) => void;
}) {
  return (
    <TextField
      select
      fullWidth={false}
      value={value}
      onChange={(e) => onChange(e.target.value)}
      slotProps={{ htmlInput: { 'aria-label': label } }}
    >
      {options.map((o) => (
        <MenuItem key={o.value} value={o.value}>
          {o.menu ?? o.label}
        </MenuItem>
      ))}
    </TextField>
  );
}

/**
 * AND / OR / NOT over leaves of one kind, to any depth, as words: a select
 * picks the leaf kind or a group; a group lists its parts in a block.
 * Switching keeps what it can.
 */
export function LogicEditor<L extends object>({
  label,
  value,
  onChange,
  leaves,
  kindOf,
  make,
  leaf,
  groups,
  empty,
  negated = false,
}: {
  label: string;
  value: Logic<L>;
  onChange: (value: Logic<L>) => void;
  /** The leaf kinds, the first one being what "Add" adds. */
  leaves: { value: string; label: string }[];
  kindOf: (leaf: L) => string;
  make: (kind: string, from?: L) => L;
  /** The words after the select, for a leaf. */
  leaf: (leaf: L, set: (leaf: L) => void) => ReactNode;
  /** How "all of", "any of" and "not" read here. */
  groups: { all: string; any: string; not: string };
  /** What an empty "all of" and an empty "any of" mean; `null`: not allowed. */
  empty: { all: string | null; any: string | null };
  /** Right inside a "not": another "not" is not offered (it would cancel out). */
  negated?: boolean;
}) {
  const node = nodeOf(value);
  const kind = node.kind === 'leaf' ? kindOf(node.leaf) : node.kind;
  const child = (one: Logic<L>, set: (one: Logic<L>) => void, inNot = false) => (
    <LogicEditor
      negated={inNot}
      label={label}
      value={one}
      onChange={set}
      leaves={leaves}
      kindOf={kindOf}
      make={make}
      leaf={leaf}
      groups={groups}
      empty={empty}
    />
  );
  const options = [
    ...leaves,
    { value: 'all', label: groups.all },
    { value: 'any', label: groups.any },
    ...(negated ? [] : [{ value: 'not', label: groups.not }]),
  ];
  let rest: ReactNode = null;
  if (node.kind === 'leaf') rest = leaf(node.leaf, onChange);
  if (node.kind === 'not') {
    rest = (
      <Block>
        <Row>{child(node.child, (next) => onChange({ not: next }), true)}</Row>
      </Block>
    );
  }
  if (node.kind === 'all' || node.kind === 'any') {
    const k = node.kind;
    const list = node.children;
    const set = (next: Logic<L>[]) => onChange(k === 'all' ? { all: next } : { any: next });
    rest = (
      <Block>
        {list.map((one, i) => (
          <Row key={i} onRemove={() => set(list.filter((_, j) => j !== i))}>
            {child(one, (next) => set(list.map((c, j) => (j === i ? next : c))))}
          </Row>
        ))}
        <Sentence>
          <Button
            color="inherit"
            startIcon={<Add />}
            onClick={() => set([...list, make(leaves[0]?.value ?? '')])}
          >
            Add
          </Button>
          {list.length === 0 && (
            <Typography variant="caption" color={empty[k] === null ? 'error' : 'text.secondary'}>
              {empty[k] === null
                ? 'Add at least one; an empty group is not allowed.'
                : `None yet: ${empty[k]}.`}
            </Typography>
          )}
        </Sentence>
      </Block>
    );
  }
  return (
    <>
      <Choice
        label={label}
        value={kind}
        options={options}
        onChange={(k) => onChange(rekind(value, k, kindOf, make))}
      />
      {rest}
    </>
  );
}
