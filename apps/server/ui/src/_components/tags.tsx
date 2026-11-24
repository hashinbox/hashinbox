import FilterList from '@mui/icons-material/FilterList';
import Autocomplete from '@mui/material/Autocomplete';
import Box from '@mui/material/Box';
import Chip from '@mui/material/Chip';
import TextField from '@mui/material/TextField';
import type { Tag } from '@/_components/tag';

/**
 * One filter box of tags, grouped, one tag per group: a newer one replaces
 * the older. Each chip names its group.
 */
export function Tags<G extends string>({
  options,
  value,
  onChange,
  placeholder,
  label,
}: {
  options: Tag<G>[];
  value: Tag<G>[];
  onChange: (tags: Tag<G>[]) => void;
  placeholder: string;
  label: string;
}) {
  return (
    <Autocomplete
      multiple
      size="small"
      options={options}
      value={value}
      groupBy={(o) => o.group}
      getOptionLabel={(o) => o.label}
      isOptionEqualToValue={(o, v) => o.group === v.group && o.value === v.value}
      onChange={(_, next) => {
        onChange(next.filter((t, i) => !next.slice(i + 1).some((u) => u.group === t.group)));
      }}
      renderValue={(tags, getItemProps) =>
        tags.map((t, index) => (
          <Chip
            {...getItemProps({ index })}
            key={`${t.group}:${t.value}`}
            size="small"
            label={
              <>
                <Box component="span" sx={{ color: 'text.secondary', mr: 0.5 }}>
                  {t.group}
                </Box>
                {t.label}
              </>
            }
            sx={{ borderRadius: '6px', bgcolor: 'grey.100', fontSize: 12 }}
          />
        ))
      }
      renderInput={(params) => (
        <TextField
          {...params}
          placeholder={value.length === 0 ? placeholder : ''}
          slotProps={{
            ...params.slotProps,
            input: {
              ...params.slotProps.input,
              startAdornment: (
                <>
                  <FilterList
                    fontSize="small"
                    aria-hidden
                    sx={{ color: 'text.secondary', ml: 0.5, mr: 0.25 }}
                  />
                  {params.slotProps.input.startAdornment}
                </>
              ),
            },
            htmlInput: { ...params.slotProps.htmlInput, 'aria-label': label },
          }}
          sx={{ '& .MuiOutlinedInput-root': { gap: 0.5, fontSize: 13 } }}
        />
      )}
      sx={{ mb: 2, maxWidth: 720 }}
    />
  );
}
