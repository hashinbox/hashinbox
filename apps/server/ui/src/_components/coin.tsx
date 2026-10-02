import Box from '@mui/material/Box';
import { mono } from '@/theme';

/** The glyph each well-known asset is written with; anything else gets its initial. */
const GLYPHS: Record<string, string> = {
  btc: '₿',
  bitcoin: '₿',
  eth: 'Ξ',
  ethereum: 'Ξ',
  sol: '◎',
  solana: '◎',
  usdc: '$',
  usdt: '₮',
  dai: '◈',
};

/** A round, single-colour badge for an asset or chain, sized to sit beside text. */
export function Coin({ symbol, size = 22 }: { symbol: string; size?: number }) {
  const key = symbol.toLowerCase();
  const glyph = GLYPHS[key] ?? key.charAt(0).toUpperCase();
  return (
    <Box
      component="span"
      aria-label={symbol}
      role="img"
      sx={{
        flex: 'none',
        width: size,
        height: size,
        borderRadius: '50%',
        border: 1,
        borderColor: 'grey.400',
        display: 'inline-grid',
        placeItems: 'center',
        fontFamily: mono,
        fontSize: size * 0.5,
        fontWeight: 500,
        lineHeight: 1,
        color: 'text.primary',
        bgcolor: 'background.paper',
      }}
    >
      {glyph}
    </Box>
  );
}
