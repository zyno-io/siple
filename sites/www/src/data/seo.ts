/**
 * Structured data (schema.org JSON-LD) for the home page. BaseLayout adds the
 * Organization on every page. Only facts the page states: no ratings, reviews,
 * prices or usage numbers.
 */
import { release } from './release';

const origin = 'https://siple.app';

export const softwareApplication = {
  '@type': 'SoftwareApplication',
  '@id': `${origin}/#software`,
  name: 'Siple',
  url: `${origin}/`,
  softwareVersion: release.version,
  applicationCategory: 'CommunicationApplication',
  applicationSubCategory: 'SIP softphone',
  operatingSystem: 'macOS 12+, Windows 10+ (x64), Windows 11 on ARM, Linux x86_64 and aarch64',
  description:
    'A native desktop SIP softphone with multiple lines, busy lamps, attended transfer, TLS and SRTP. Every action is also a JSON-RPC method, with an agent skill and an MCP server, so scripts and AI agents can drive the same calls.',
  license: 'https://github.com/zyno-io/siple/blob/main/LICENSE',
  downloadUrl: 'https://github.com/zyno-io/siple/releases',
  publisher: { '@id': `${origin}/#organization` },
  sameAs: ['https://github.com/zyno-io/siple'],
};

export const webSite = {
  '@type': 'WebSite',
  '@id': `${origin}/#website`,
  name: 'Siple',
  url: `${origin}/`,
  publisher: { '@id': `${origin}/#organization` },
};
