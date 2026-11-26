import Box from '@mui/material/Box';
import Stack from '@mui/material/Stack';
import Typography from '@mui/material/Typography';
import type { ReactNode } from 'react';
import { HashinboxMark } from '@/_components/logo';
import { Shader } from '@/_components/shader';

/**
 * The logged-out page: a black-and-white shader scene with the instance's
 * name, and a panel holding the form (an overline, a title, a line,
 * then it). On a phone the scene folds into a band above the form.
 */
export function Welcome({
  name,
  overline,
  title,
  note,
  children,
}: {
  name: string;
  overline: string;
  title: string;
  note: ReactNode;
  children: ReactNode;
}) {
  return (
    <Box
      component="main"
      sx={{ flexGrow: 1, display: 'flex', flexDirection: { xs: 'column', md: 'row' } }}
    >
      <Box
        sx={{
          position: 'relative',
          overflow: 'hidden',
          flexGrow: { md: 1 },
          display: 'flex',
          alignItems: 'center',
          minHeight: { xs: 180, md: 'auto' },
          bgcolor: '#000',
          color: '#fff',
        }}
      >
        <Shader />
        {/* A soft dark wash behind the text keeps it readable over the light. */}
        <Box
          aria-hidden
          sx={{
            position: 'absolute',
            inset: 0,
            background:
              'radial-gradient(ellipse 70% 60% at 25% 50%, rgba(0,0,0,.72), rgba(0,0,0,0) 75%)',
          }}
        />
        <Stack
          spacing={{ xs: 1, md: 2 }}
          sx={{ position: 'relative', px: { xs: 2, md: 8 }, py: { xs: 3, md: 8 } }}
        >
          <Box sx={{ display: 'flex', height: { xs: 32, md: 52 } }}>
            <HashinboxMark height={52} color="#fff" style={{ height: '100%', width: 'auto' }} />
          </Box>
          <Typography variant="h1" sx={{ fontSize: { xs: 30, md: 44 } }}>
            {name}
          </Typography>
          <Typography sx={{ maxWidth: 320, color: 'rgba(255,255,255,.66)' }}>
            Receive, track and settle crypto payments
          </Typography>
        </Stack>
      </Box>
      <Box
        sx={{
          width: { xs: '100%', md: 480 },
          flexShrink: 0,
          flexGrow: { xs: 1, md: 0 },
          display: 'flex',
          alignItems: { md: 'center' },
          bgcolor: 'background.paper',
          borderLeft: { md: 1 },
          borderTop: { xs: 1, md: 0 },
          borderColor: 'divider',
          px: { xs: 2, sm: 4, md: 6 },
          py: { xs: 4, md: 6 },
        }}
      >
        <Box sx={{ width: '100%', maxWidth: 400, mx: 'auto' }}>
          <Typography variant="overline" component="p">
            {overline}
          </Typography>
          <Typography variant="h2" sx={{ mt: 0.5 }}>
            {title}
          </Typography>
          <Typography variant="body2" color="text.secondary" sx={{ mt: 1, mb: 3 }}>
            {note}
          </Typography>
          {children}
        </Box>
      </Box>
    </Box>
  );
}
