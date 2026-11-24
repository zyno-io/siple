# Send a setup link

Give people a link that fills in their Siple account, so they only type a password.

A **setup link** starts with `siple:`. It carries the non-secret parts of an account: who the user is and which server to use.
It never carries a password. When someone opens it, Siple shows what it would set up and waits. Nothing is saved or sent until
the person has read it, continued, typed their password and run the test.

## What a link carries

A link carries these nine account fields and nothing else. `username` and `domain` are required.

| Field | Meaning | Values |
|---|---|---|
| `username` | SIP user name or extension | 1 to 64 characters; letters, digits and `-_.!~*'()&=+$,` |
| `domain` | SIP domain | a host name, a full IPv4 address (`192.0.2.10`) or `[IPv6]`, no port; stored in lowercase. IPv4 shorthand such as `127.1`, `10.1`, `2130706433` or `0x7f.1` is refused |
| `display_name` | Name shown to the other party | up to 64 characters; no quotes (`"`), backslashes or angle brackets (`<` `>`), and no control, invisible or text-direction characters (this includes emoji variation selectors such as U+FE0F, so a few emoji lose their color form) |
| `auth_username` | Sign-in name, when it differs from `username` | same rules as `username` |
| `registrar` | Registrar server | `host` or `host:port`, without `sip:`; same host rules as `domain` |
| `outbound_proxy` | Outbound proxy | `host` or `host:port`; same host rules as `domain` |
| `transport` | How to reach the server | `udp`, `tcp`, `tls` or `auto` (default `auto`) |
| `srtp` | Call encryption | `off`, `optional` or `mandatory` (default `optional`) |
| `profile` | Desk-phone profile | `generic`, `yealink`, `polycom`, `cisco` or `grandstream` (default `generic`) |

A link never carries a password, and Siple refuses a link that tries to. Every other setting is refused, not ignored: a link
cannot turn off certificate checks, pin a certificate, switch on auto-answer or intercom, add SIP headers, choose audio
devices, or change anything outside the account. Those stay with the person (or with `config.toml`). Not in a link: BLF keys,
codecs, a voicemail number, a separate port (put it in `registrar` or `outbound_proxy` as `host:port`) and an account id (Siple
derives it from the user name and domain).

## The format

A link is `siple:provision?d=` followed by compact JSON in base64url (RFC 4648 section 5, no padding):

```json
{"v":1,"account":{"username":"1001","domain":"example.test","display_name":"Front desk","registrar":"pbx.example.test:5061","transport":"tls","srtp":"mandatory"}}
```

```text
siple:provision?d=eyJ2IjoxLCJhY2NvdW50Ijp7InVzZXJuYW1lIjoiMTAwMSIsImRvbWFpbiI6ImV4YW1wbGUudGVzdCIsImRpc3BsYXlfbmFtZSI6IkZyb250IGRlc2siLCJyZWdpc3RyYXIiOiJwYnguZXhhbXBsZS50ZXN0OjUwNjEiLCJ0cmFuc3BvcnQiOiJ0bHMiLCJzcnRwIjoibWFuZGF0b3J5In19
```

- `v` is the version and must be `1`. A link with a higher version asks the user to update Siple.
- Every value is a string, except `v`. An empty string means "not set".
- A whole link is at most 2048 bytes. The link must have exactly one query parameter, `d`.
- Keys must not repeat.

## Build a link

Pick whichever fits your workflow.

1. **In Siple.** Open **Settings › Accounts**, choose an account, then **Copy setup link**. The link is copied to the clipboard
   and has no password. Siple lists the settings it left out, such as the password.
2. **On the command line, offline.** `siple-cli setup-link build` takes the fields as JSON (inline, `@file.json` or `-` for
   stdin) and prints the link. `siple-cli setup-link parse '<link>'` shows what a link would set up.

   ```sh
   siple-cli setup-link build '{"username":"1001","domain":"example.test","transport":"tls"}'
   ```
3. **From a running Siple.** `siple-cli ctl provision.build '{"fields":{"username":"1001","domain":"example.test"}}'`, or
   `'{"account":"desk"}'` for an account in the config.
4. **From a shell script.** Encode the JSON yourself:

   ```sh
   json='{"v":1,"account":{"username":"1001","domain":"example.test"}}'
   printf 'siple:provision?d=%s\n' "$(printf '%s' "$json" | base64 | tr '+/' '-_' | tr -d '=\n')"
   ```

Put the link in an email, a chat message, an intranet page, or a QR code. A link contains the user name and server names, so
treat it as semi-private.

## What the person sees

They click the link, or choose **Paste a setup link** in the setup (**Add from setup link** in **Settings › Accounts**). Siple
shows **Check this setup link**:

<Shot name="setup-review" focus="314,0,612,470" alt="The review screen: the server in large type, a warning that it is outside the domain, and the account details" caption="The review screen leads with the server, because that is where the password and the calls would go." />

1. **Read.** The server is shown large. Siple warns when the registrar or proxy is outside the account's domain, is an IP
   address, is this computer (`localhost`, `127.0.0.1`, `[::1]`, `0.0.0.0`), uses an encoded international name, or when SRTP is off.
2. **Continue.** The button works one second after the screen appears (and again one second after the link changes).
3. **Fill in the password.** The form is prefilled from the link. The person types the password; **Test** stays disabled until a password is typed.
4. **Test.** Siple signs in once on a private connection. Only when that works is the account saved.

If the person already has that account (same user and domain), Siple says so and does not add it twice. A link cannot update an
existing account.

## Security notes

- **No password in a link, ever.** Links travel through mail, chat previews, browser history and command lines.
- **Nothing happens until the person acts.** Siple makes no network request and writes nothing while the review screen is
  showing. The sign-in happens only after Continue, a typed password and **Test & continue** (**Test & save** when adding to a configured install).
- **Allow-list, not block-list.** Only the nine fields above are accepted. Unknown and sensitive keys make the link fail.
- **A new link never replaces work in progress.** Once the person has continued past the review screen, or a test is running,
  a new link is refused with the notice "Finish or cancel the current setup first" and the form is left alone. A link on the
  review screen is replaced, with a warning, and Continue locks for a second again.
- **Server names are ASCII.** Look-alike letters are refused; names that use the `xn--` encoding are shown as written and
  flagged.
- **Agents.** `app.open_uri` can open the review screen but cannot skip it. A headless script can call `provision.parse`,
  `account.test` and `config.edit` itself, which needs the control token it already holds.

::: details In config.toml
The account a link creates is an ordinary `[[account]]` block, written by `config.edit` `add_account` after the test passes.
The id is `<username>.<domain>` with unusual characters replaced by `-`. The methods are `provision.parse` and
`provision.build`; see the [API reference](/agents/api-reference). Links and the `handle_links` switch are described in
[Phone links](/guide/links).
:::
