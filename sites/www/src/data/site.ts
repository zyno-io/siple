/**
 * Site-wide links, release URLs and launch flags. One place to edit when
 * a store listing goes live or the domain is chosen.
 */
const repo = 'https://github.com/zyno-io/siple';
const docs = 'https://zyno-io.github.io/siple';

export const site = {
  name: 'Siple',
  tagline: 'The scriptable SIP softphone',
  maker: 'Zyno Consulting',
  legalName: 'Signal24 LLC dba Zyno Consulting',
  contact: 'oss@zyno.io',
  repo,
  releases: `${repo}/releases`,
  licenceText: `${repo}/blob/main/LICENSE`,
  verify: `${repo}/blob/main/docs/RELEASING.md#verify-a-download`,
  docs: `${docs}/`,
  privacy: '/privacy/',
  terms: 'https://zyno.consulting/terms/',
  zynotalk: 'https://zyno.talk',
} as const;

/** Docs pages the site links to (VitePress, clean URLs). */
export const doc = {
  install: `${docs}/guide/install`,
  firstRun: `${docs}/guide/first-run`,
  calls: `${docs}/guide/calls`,
  transfers: `${docs}/guide/transfers`,
  extensions: `${docs}/guide/extensions`,
  shortcuts: `${docs}/guide/shortcuts`,
  tray: `${docs}/guide/tray`,
  links: `${docs}/guide/links`,
  setupLinks: `${docs}/guide/setup-links`,
  problems: `${docs}/guide/troubleshooting#what-siple-s-messages-mean`,
  network: `${docs}/guide/network`,
  configuration: `${docs}/guide/configuration`,
  agents: `${docs}/agents/`,
  skill: `${docs}/agents/skill`,
  mcp: `${docs}/agents/mcp-quickstart`,
  jsonRpc: `${docs}/agents/json-rpc`,
  api: `${docs}/agents/api-reference`,
  configRef: `${docs}/reference/configuration`,
  licence: `${docs}/project/licence`,
  changelog: `${docs}/project/changelog`,
  security: `${docs}/project/security`,
  buildFromSource: `${docs}/guide/install#build-from-source`,
} as const;

/**
 * Release links, fixed at build time for the version in src/data/release.ts (the newest CHANGELOG section). Production
 * is deployed only after every link checks live (.github/workflows/www.yml);
 * nothing is looked up in the browser.
 */
export const releasePage = (version: string) => `${repo}/releases/tag/v${version}`;
export const assetUrl = (version: string, name: string) => `${repo}/releases/download/v${version}/${name}`;
/** Release assets are named `Siple-<version>-<suffix>` (release.yml). */
export const assetName = (version: string, suffix: string) => `Siple-${version}-${suffix}`;
export const checksumsUrl = (version: string) => assetUrl(version, 'SHA256SUMS');

export type PlatformId = 'mac' | 'win-x64' | 'win-arm64' | 'linux' | 'linux-arm64';

export interface Platform {
  id: PlatformId;
  /** Font Awesome brand icon. */
  icon: string;
  name: string;
  /** Short label for the primary button: "Download for {short}". */
  short: string;
  requires: string;
  /** Release asset name suffix (`Siple-<version>-<suffix>`). */
  suffix: string;
  /** An alternative build for the same platform (Linux: the tarball), linked beside the primary one. */
  alt?: { suffix: string; label: string; file: string };
  /** One line of honest how-to shown under the row (from the install guide). */
  note?: string;
  /** File type of the primary build. */
  kind: string;
  /** How this build is signed, as release.yml / docs/RELEASING.md describe it. */
  trust: string;
}

/** From the install guide: the AppImage updates itself, the tarball suits servers and agents. */
const linuxNote =
  'AppImage: chmod +x and run; it updates itself in place. Tarball: for servers and agents.';

export const platforms: Platform[] = [
  {
    id: 'mac',
    icon: 'fa-apple',
    name: 'macOS',
    short: 'macOS',
    requires: 'macOS 12 or later · Universal (Apple silicon and Intel)',
    suffix: 'macos.dmg',
    kind: '.dmg',
    trust: 'Notarized',
  },
  {
    id: 'win-x64',
    icon: 'fa-windows',
    name: 'Windows x64',
    short: 'Windows',
    requires: 'Windows 10 or later · Intel and AMD PCs',
    suffix: 'windows-x64.zip',
    kind: '.zip',
    trust: 'Authenticode-signed',
  },
  {
    id: 'win-arm64',
    icon: 'fa-windows',
    name: 'Windows ARM64',
    short: 'Windows on ARM',
    requires: 'Windows 11 on ARM · Snapdragon and Surface Pro X / 11',
    suffix: 'windows-arm64.zip',
    kind: '.zip',
    trust: 'Authenticode-signed',
  },
  {
    id: 'linux',
    icon: 'fa-linux',
    name: 'Linux x86_64',
    short: 'Linux',
    requires: 'glibc 2.36 or later · Debian 12+, Ubuntu 23.04+',
    suffix: 'linux-x86_64.AppImage',
    alt: { suffix: 'linux-x86_64.tar.gz', label: 'Tarball', file: 'x86_64 tarball (.tar.gz)' },
    kind: '.AppImage',
    note: linuxNote,
    trust: 'SHA-256 checksum',
  },
  {
    id: 'linux-arm64',
    icon: 'fa-linux',
    name: 'Linux on ARM',
    short: 'Linux on ARM',
    requires: 'glibc 2.36 or later · Debian 12+, Ubuntu 23.04+ · aarch64',
    suffix: 'linux-aarch64.AppImage',
    alt: { suffix: 'linux-aarch64.tar.gz', label: 'Tarball', file: 'aarch64 tarball (.tar.gz)' },
    kind: '.AppImage',
    note: linuxNote,
    trust: 'SHA-256 checksum',
  },
];

/**
 * App store listings. All are planned and none exists yet, so they render
 * as disabled "Coming soon" buttons drawn in the design system (never Apple's,
 * Microsoft's badge artwork for a listing that isn't live).
 * Flathub is not listed: the submission is deferred (Siple docs/STORE.md,
 * "Flathub"), and the install guide no longer promises it.
 *
 * At launch: set `available: true` and `url`. To use the official badge,
 * also drop the artwork Apple / Microsoft supply into public/badges/ and set
 * `badge` to its path; without `badge` a live listing renders as a normal
 * design-system link button.
 */
export interface Store {
  id: 'mac-app-store' | 'microsoft-store';
  name: string;
  icon: string;
  available: boolean;
  url: string;
  badge?: string;
  badgeAlt?: string;
}

export const stores: Store[] = [
  {
    id: 'mac-app-store',
    name: 'Mac App Store',
    icon: 'fa-apple',
    available: false,
    url: '',
    // badge: '/badges/mac-app-store.svg',
    // badgeAlt: 'Download on the Mac App Store',
  },
  {
    id: 'microsoft-store',
    name: 'Microsoft Store',
    icon: 'fa-microsoft',
    available: false,
    url: '',
    // badge: '/badges/microsoft-store.svg',
    // badgeAlt: 'Get it from Microsoft',
  },
];
