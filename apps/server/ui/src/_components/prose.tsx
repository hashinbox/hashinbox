import Box from '@mui/material/Box';
import Typography from '@mui/material/Typography';
import { Fragment } from 'react';
import { mono } from '@/theme';

/**
 * Text written as Markdown-lite: blank lines part paragraphs, single line
 * breaks are wrapping, and `backticks` mark code. Nothing else is interpreted,
 * so nothing a document says can become markup.
 */
export function Prose({ text, size = 13.5 }: { text: string; size?: number }) {
  const paragraphs = text
    .split(/\n\s*\n/)
    .map((p) => p.replace(/\s*\n\s*/g, ' ').trim())
    .filter(Boolean);
  return (
    <>
      {paragraphs.map((paragraph, i) => (
        <Typography
          key={i}
          color="text.secondary"
          sx={{ fontSize: size, lineHeight: 1.65, maxWidth: '72ch', '& + &': { mt: 1.25 } }}
        >
          {paragraph.split(/(`[^`]+`)/).map((part, j) =>
            part.startsWith('`') && part.endsWith('`') && part.length > 1 ? (
              <Box
                key={j}
                component="code"
                sx={{
                  fontFamily: mono,
                  fontSize: '0.88em',
                  color: 'text.primary',
                  px: 0.5,
                  border: 1,
                  borderColor: 'divider',
                }}
              >
                {part.slice(1, -1)}
              </Box>
            ) : (
              <Fragment key={j}>{part}</Fragment>
            ),
          )}
        </Typography>
      ))}
    </>
  );
}
