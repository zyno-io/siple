// Copies the GUI snapshot PNGs used by the docs from the Rust visual tests into
// website/public/screenshots/ (git-ignored), so the site always shows the real UI
// and no duplicate binaries are committed.
import { copyFileSync, existsSync, mkdirSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const src = resolve(here, '../../crates/siple/tests/snapshots')
const dst = resolve(here, '../public/screenshots')

// name -> snapshot file stem (without _light/_dark_wide.png)
export const shots = {
  idle: 'idle',
  'two-lines': 'two_lines',
  'call-waiting': 'call_waiting',
  'transfer-consult': 'transfer_consult',
  'blf-keys': 'blf_keys',
  'settings-accounts': 'settings_accounts',
  'wizard-account': 'wizard_account'
}

mkdirSync(dst, { recursive: true })
let missing = 0
for (const [name, stem] of Object.entries(shots)) {
  for (const theme of ['light', 'dark']) {
    const from = join(src, `${stem}_${theme}_wide.png`)
    if (!existsSync(from)) {
      console.error(`missing snapshot: ${from}`)
      missing++
      continue
    }
    copyFileSync(from, join(dst, `${name}-${theme}.png`))
  }
}
if (missing) process.exit(1)
console.log(`copied ${Object.keys(shots).length * 2} screenshots to ${dst}`)
