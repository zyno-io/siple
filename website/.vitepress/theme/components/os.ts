// The visitor's desktop OS, for showing the right keys and install steps. Client-only: call it in
// onMounted, so the server-rendered page (which shows the Windows/Linux variant) hydrates cleanly.
export type Os = 'mac' | 'windows' | 'linux'

export function detectOs(): Os {
  const nav = navigator as Navigator & { userAgentData?: { platform?: string } }
  const p = `${nav.userAgentData?.platform ?? ''} ${nav.platform ?? ''} ${nav.userAgent ?? ''}`.toLowerCase()
  if (p.includes('mac') || p.includes('iphone') || p.includes('ipad')) return 'mac'
  if (p.includes('win')) return 'windows'
  return 'linux'
}
