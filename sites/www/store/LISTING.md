# Store listings

Copy for App Store Connect and Partner Center. Screenshots: `mac/` (2880×1800) and `windows/` (3840×2160),
uploaded in file order. Regenerate with `npm run store-shots` (see the README).

Rules for this copy:
- Claims come from the Siple repo. ZynoTalk claims come from ZynoTalk's own site.
- Say that Siple works with any SIP provider. Never imply ZynoTalk is required.
- The Mac copy must not mention Windows or Linux (App Review 2.3.10).
- The Mac App Store edition has no speech hooks, so neither listing promises text-to-speech or transcription.

The demo account's extension, SIP domain and password are not in this public file. They are the ZynoTalk
App Review endpoint in the Zyno demo tenant; fill in the `<DEMO …>` placeholders when pasting.

---

## Mac App Store (App Store Connect)

| Field | Value |
|---|---|
| Name | Siple |
| Subtitle (30) | Scriptable SIP softphone |
| Primary category | Business |
| Secondary category | Developer Tools |
| Price | Free |
| Support URL | https://github.com/zyno-io/siple/issues |
| Marketing URL | https://siple.app/ |
| Privacy policy URL | https://siple.app/privacy/ |
| Copyright | 2026 Signal24 LLC |
| App Privacy | Data Not Collected |
| Availability | All territories except France |

### Promotional text (170)

A native SIP softphone for your business phone system. Every action is also open to scripts and AI agents,
through a command-line tool and an MCP server.

### Keywords (100)

```
voip,pbx,phone,calls,dialer,blf,busy lamp,srtp,mcp,ai agent,cli,api,zynotalk,extension,transfer
```

### Description

```
Siple is a native SIP softphone for your Mac. Sign in with the SIP account from your phone system or provider, then make and take calls from your desk. Anything you can do in the window, a script or an AI agent can do too.

FOR YOUR DESK
• Several accounts and lines in one window, with hold, mute, call waiting and do not disturb.
• Blind, attended and semi-attended transfer.
• Busy lamps (BLF) for your colleagues' extensions, with one-click pickup when one is ringing.
• Voicemail lamps.
• Opus HD audio, G.722 and G.711, with DTMF.
• Encrypted calls: TLS for signalling and SRTP for audio.
• Light and dark themes that follow macOS.
• Opens sip:, tel: and callto: links.

FOR SCRIPTS AND AI AGENTS
• Every action and every piece of state in the app is a JSON-RPC method on a local control interface.
• The bundled siple-cli command drives Siple from Terminal and shell scripts.
• Siple includes an MCP server, so AI agents can place, answer and transfer calls, send DTMF and watch busy lamps.
• Agents can play audio files into a call, record calls and stream call audio to their own programs.
• Only your own user account can reach the control interface.

WORKS WITH YOUR PROVIDER
Siple works with any standards-based PBX or SIP provider. It supports UDP, TCP and TLS, DNS SRV, outbound proxies and NAT keep-alives. It re-registers by itself after sleep or a network change.

NEED A PHONE LINE?
If you don't have a SIP provider yet, ZynoTalk is our cloud phone system for businesses: HD audio, two-way SMS, voicemail and fax on real business numbers, with AI summaries, caller pop and voice agents built in. Siple works with a ZynoTalk extension out of the box. Learn more at zyno.talk.

PRIVATE BY DESIGN
Siple has no account with us, no telemetry, no analytics and no ads. Your settings, logs and recordings stay on your Mac, and passwords are kept in your Keychain. Siple connects only to the SIP servers you set up.

Siple's source code is public at github.com/zyno-io/siple.
```

### App Review notes

Field values for "Sign-in required": username = the demo extension, password = the demo endpoint password.

```
Siple is a SIP softphone. It needs a SIP account on a phone system to place calls. We have set up a demo account on ZynoTalk, our own cloud phone system.

DEMO ACCOUNT
On first launch Siple shows "Add account". Enter:
• Username / extension: <DEMO EXTENSION>
• Password: (see the Sign-in information fields)
• Domain / server: <DEMO SIP DOMAIN>
• Click "Advanced options" and set Transport to TLS. Leave the other options as they are.
Click "Add account". The account chip at the top of the window turns green when it is registered.

TEST CALLS
• Dial 123 to reach a test menu (IVR). It answers and plays a recorded prompt.
• Dial *43 for an echo test: what you say is played back to you.
Calls are encrypted (TLS and SRTP).

PERMISSIONS AND ENTITLEMENTS
• Microphone: used only to send your voice to the other party during a call.
• Outgoing network: SIP signalling and call audio to the server the user configures.
• Incoming network: call audio (RTP) arrives on local UDP ports. There is also an optional TCP control listener, off by default and protected by a token.
• App group: the bundled siple-cli helper reaches the running app through a socket in the shared group container.

The app collects no data. Privacy policy: https://siple.app/privacy/
Contact: oss@zyno.io
```

---

## Microsoft Store (Partner Center)

| Field | Value |
|---|---|
| Product name | Siple |
| Category | Business |
| Subcategory | Collaboration |
| Price | Free |
| Markets | All |
| Website | https://siple.app/ |
| Support contact info | oss@zyno.io |
| Privacy policy URL | https://siple.app/privacy/ |
| Copyright and trademark info | © 2026 Signal24 LLC dba Zyno Consulting |
| Age rating (IARC) | Answer "yes" only to users being able to talk to each other. Everything else is "no". |

### Short description (shown above the fold; up to 1,000)

```
A native SIP softphone for your business phone system. Several lines, busy lamps, transfers and encrypted calls in one window, and every action is open to scripts and AI agents through siple-cli and an MCP server. Works with any SIP provider, including ZynoTalk.
```

### Description

```
Siple is a native SIP softphone for Windows. Sign in with the SIP account from your phone system or provider, then make and take calls from your desk. Anything you can do in the window, a script or an AI agent can do too.

FOR YOUR DESK
• Several accounts and lines in one window, with hold, mute, call waiting and do not disturb.
• Blind, attended and semi-attended transfer.
• Busy lamps (BLF) for your colleagues' extensions, with one-click pickup when one is ringing.
• Voicemail lamps.
• Opus HD audio, G.722 and G.711, with DTMF.
• Encrypted calls: TLS for signalling and SRTP for audio.
• Light and dark themes that follow Windows.
• Opens sip:, tel: and callto: links, and can start with Windows.

FOR SCRIPTS AND AI AGENTS
• Every action and every piece of state in the app is a JSON-RPC method on a local control interface.
• The siple-cli command works from PowerShell and Command Prompt as soon as Siple is installed.
• Siple includes an MCP server, so AI agents can place, answer and transfer calls, send DTMF and watch busy lamps.
• Agents can play audio files into a call, record calls and stream call audio to their own programs.
• Only your own user account can reach the control interface.

WORKS WITH YOUR PROVIDER
Siple works with any standards-based PBX or SIP provider. It supports UDP, TCP and TLS, DNS SRV, outbound proxies and NAT keep-alives. It re-registers by itself after sleep or a network change.

NEED A PHONE LINE?
If you don't have a SIP provider yet, ZynoTalk is our cloud phone system for businesses: HD audio, two-way SMS, voicemail and fax on real business numbers, with AI summaries, caller pop and voice agents built in. Siple works with a ZynoTalk extension out of the box. Learn more at zyno.talk.

PRIVATE BY DESIGN
Siple has no account with us, no telemetry, no analytics and no ads. Your settings, logs and recordings stay on your PC, and passwords are kept in Windows Credential Manager. Siple connects only to the SIP servers you set up.

Siple's source code is public at github.com/zyno-io/siple. It runs natively on both x64 and Arm PCs.
```

### Product features (up to 20, 200 each)

```
Several SIP accounts and lines in one window
Hold, mute, call waiting and do not disturb
Blind, attended and semi-attended transfer
Busy lamps (BLF) with one-click pickup
Voicemail lamps
Opus HD audio, G.722 and G.711
Encrypted calls with TLS and SRTP
JSON-RPC control API for every action and state
siple-cli command for PowerShell and scripts
MCP server for AI agents
Works with any SIP provider, including ZynoTalk
No telemetry, no analytics, no ads
Native on x64 and Arm
```

### Search terms (7 max, 30 chars each)

```
SIP softphone
VoIP
PBX
business phone
MCP AI agent
ZynoTalk
BLF
```

### Screenshot captions (200 each, in file order)

1. The scriptable SIP softphone. Several lines, hold, mute and encrypted calls in one window.
2. Never miss a call. Answer, reject or silence an incoming call straight from its line.
3. Attended transfer, right in the call. Talk to the target first, then complete the transfer or go back.
4. See who's busy at a glance. Busy lamps for every extension, grouped and sortable.
5. Light or dark, like your desktop.

### Additional system requirements

- Minimum hardware: a microphone and speakers or a headset.
- Recommended: a SIP account on a phone system or with a provider.

### Notes for certification

```
Siple is a SIP softphone. It needs a SIP account on a phone system to place calls. We have set up a demo account on ZynoTalk, our own cloud phone system.

DEMO ACCOUNT
On first launch Siple shows "Add account". Enter:
• Username / extension: <DEMO EXTENSION>
• Password: <DEMO PASSWORD>
• Domain / server: <DEMO SIP DOMAIN>
• Click "Advanced options" and set Transport to TLS. Leave the other options as they are.
Click "Add account". The account chip at the top of the window turns green when it is registered.

TEST CALLS
• Dial 123 to reach a test menu (IVR). It answers and plays a recorded prompt.
• Dial *43 for an echo test: what you say is played back to you.
Calls are encrypted (TLS and SRTP).

WHY runFullTrust
Siple is a native Win32 app (Rust), not a UWP app. It needs full trust to:
• send and receive SIP and RTP over raw UDP and TCP sockets, on ports chosen at runtime;
• use the audio devices directly through WASAPI for low-latency calls;
• serve its local control interface (a named pipe, reachable only by the signed-in user) to the siple-cli console command, which the package installs as an app execution alias.

FIREWALL RULES
The package declares inbound UDP and TCP firewall rules for siple.exe so call audio and incoming calls work behind Windows Firewall. The rules apply to siple.exe only.

The app collects no data. Privacy policy: https://siple.app/privacy/
Contact: oss@zyno.io
```
