// One theme for both modes: ink on paper by day, paper on ink by night.
// Hairlines instead of fills, square corners, a wide geometric face for what
// matters, mono for anything that must be read exactly, and a faint dot grid
// underneath, like graph paper.
import { createTheme, type Theme } from '@mui/material/styles';

export const display = '"Unbounded", "Geist", "Helvetica Neue", Arial, sans-serif';
export const mono = '"Geist Mono", ui-monospace, "SF Mono", Menlo, Consolas, monospace';
const text = '"Geist", "Helvetica Neue", Arial, sans-serif';

export function theme(mode: 'light' | 'dark'): Theme {
  const light = mode === 'light';
  const ink = light ? '#0b0b0b' : '#f1f1ef';
  const paper = light ? '#ffffff' : '#000000';
  const mute = light ? '#6f6f6f' : '#8d8d8a';
  const line = light ? '#e3e3e1' : '#232323';
  const rule = light ? '#c9c9c6' : '#353535';
  const hover = light ? '#f3f3f1' : '#111111';

  return createTheme({
    palette: {
      mode,
      primary: { main: ink, contrastText: paper },
      background: { default: paper, paper },
      text: { primary: ink, secondary: mute },
      divider: line,
      grey: { 400: rule, 300: line },
      action: { hover, selected: hover },
      warning: { main: light ? '#8a6d00' : '#e2b93b' },
      error: { main: light ? '#b3261e' : '#ff6b5f' },
      // Status lights are the one place colour is allowed to mean something.
      success: { main: light ? '#1f8a4c' : '#3ccf7e' },
    },
    shape: { borderRadius: 0 },
    typography: {
      fontFamily: text,
      fontSize: 14,
      h1: {
        fontFamily: display,
        fontWeight: 500,
        fontSize: 44,
        lineHeight: 1,
        letterSpacing: '-.03em',
      },
      h2: { fontFamily: display, fontWeight: 500, fontSize: 26, letterSpacing: '-.02em' },
      h3: { fontFamily: display, fontWeight: 500, fontSize: 17, letterSpacing: '-.01em' },
      h4: { fontFamily: display, fontWeight: 500, fontSize: 14 },
      overline: {
        fontFamily: mono,
        fontSize: 10.5,
        fontWeight: 400,
        letterSpacing: '.14em',
        lineHeight: 1.6,
        color: mute,
      },
      button: { textTransform: 'none', fontWeight: 500, letterSpacing: 0 },
    },
    components: {
      MuiCssBaseline: {
        styleOverrides: {
          body: { WebkitFontSmoothing: 'antialiased' },
          '::selection': { background: ink, color: paper },
        },
      },
      MuiButton: {
        defaultProps: { disableElevation: true, variant: 'outlined', size: 'small' },
        styleOverrides: {
          root: { borderRadius: 999, paddingInline: 14 },
          outlined: { borderColor: rule },
        },
      },
      MuiPaper: {
        defaultProps: { elevation: 0, square: true },
        styleOverrides: { root: { backgroundImage: 'none' } },
      },
      MuiAppBar: {
        defaultProps: { elevation: 0, color: 'inherit', position: 'sticky' },
        styleOverrides: { root: { borderBottom: `1px solid ${line}` } },
      },
      MuiDialog: {
        styleOverrides: { paper: { border: `1px solid ${rule}` } },
      },
      MuiTextField: { defaultProps: { variant: 'standard', fullWidth: true } },
      MuiInputBase: { styleOverrides: { input: { fontFamily: mono, fontWeight: 300 } } },
      MuiTab: {
        styleOverrides: {
          root: {
            textTransform: 'none',
            minWidth: 0,
            paddingInline: 12,
            color: mute,
            '&.Mui-selected': { color: ink },
          },
        },
      },
      MuiTabs: { styleOverrides: { indicator: { display: 'none' } } },
      MuiTable: { defaultProps: { size: 'small' } },
      MuiTableCell: {
        styleOverrides: {
          root: {
            borderBottomColor: line,
            fontSize: 12,
            padding: '8px 12px',
            '&:first-child': { paddingLeft: 0 },
            '&:last-child': { paddingRight: 0 },
          },
          head: {
            paddingTop: 6,
            paddingBottom: 6,
            fontFamily: mono,
            fontSize: 10.5,
            letterSpacing: '.12em',
            textTransform: 'uppercase',
            color: mute,
            fontWeight: 500,
            borderBottomColor: rule,
            whiteSpace: 'nowrap',
          },
        },
      },
      MuiTableRow: { styleOverrides: { root: { '&:last-child td': { borderBottom: 0 } } } },
      MuiListItemButton: {
        styleOverrides: {
          root: {
            borderTop: `1px solid ${line}`,
            '&.Mui-selected, &.Mui-selected:hover': { backgroundColor: hover, color: ink },
          },
        },
      },
      MuiAccordion: {
        defaultProps: { disableGutters: true, square: true },
        styleOverrides: { root: { border: `1px solid ${line}`, '&::before': { display: 'none' } } },
      },
      MuiChip: {
        styleOverrides: {
          root: { borderRadius: 999, fontFamily: mono, fontSize: 10.5, letterSpacing: '.12em' },
        },
      },
      MuiAlert: { styleOverrides: { root: { borderRadius: 0 } } },
    },
  });
}
