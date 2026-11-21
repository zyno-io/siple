# Questions and answers

Short answers to the questions people ask most about Siple.

## Getting started

### What do I need to use Siple?

You need a computer and a SIP account: a user name, a password and a server address from your IT team or phone provider.
Siple runs on macOS 12 or later, Windows 10 or later (Windows 11 on ARM) and Linux (Debian 12+, Ubuntu 23.04+). See
[Install](/guide/install) and [Set up your account](/guide/first-run).

### Which phone systems does it work with?

Siple works with any standards-based PBX (phone system) or SIP provider: Asterisk and FreePBX, FreeSWITCH, platforms fronted
by Kamailio, and hosted UCaaS (cloud phone) services. If a desk phone or another softphone can sign in with a user name,
password and server, Siple can too.

### Is Siple a web app wrapped in a window?

No. Siple is a native app written in Rust with the egui toolkit: no Electron, no embedded browser, and no OpenSSL.


### Can I use more than one account?

Yes. You can add several accounts in **Settings › Accounts** with **New account**. Siple handles several calls at once, on
one or more accounts. See [Make and answer calls](/guide/calls#switch-between-lines).

### Is there a Mac App Store or Microsoft Store version?

Both are coming soon. Until then, download Siple from [siple.app](https://siple.app/#download) or
[GitHub Releases](https://github.com/zyno-io/siple/releases/latest).

## Privacy and security

### Where are my settings stored?

In one file, `config.toml`, in your user folder:

| OS | Default config file |
|---|---|
| macOS | `~/Library/Application Support/Siple/config.toml` |
| Windows | `%APPDATA%\Siple\config.toml` |
| Linux | `$XDG_CONFIG_HOME/siple/config.toml` |
| Linux (Flatpak) | `~/.var/app/io.zyno.siple/config/siple/config.toml` |

Choose **Open config** at the bottom of the window to open it. The settings pages and the file change the same thing, and
Siple picks up edits while it runs. For everything in the file, see the
[configuration reference](/reference/configuration).

### Is my password stored safely?

By default the password is stored in `config.toml`, which is readable only by you. If you prefer, turn on **Store in system
keychain** when you add the account, or **Store password in system keychain** in **Settings › Accounts**. The password then
lives in the macOS Keychain, Windows Credential Manager or the Linux Secret Service. See
[Store the password in the keychain](/guide/first-run#store-the-password-in-the-keychain).

### Does Siple collect data about me?

No. Siple has no telemetry. The only network activity it starts on its own is an optional, anonymous check for a newer
version, and you can turn it off in **Settings › Advanced** with **Check for updates**. Details: [Updates](/guide/updates).

## Using Siple

### How do I update Siple?

When a newer version exists, an **Update** pill appears in the top bar. With a direct download, choose **Install and restart**.
Siple downloads the new version, checks it and installs it, and never restarts during a call. See [Updates](/guide/updates).

### Can I control Siple from scripts or AI agents?

Yes. Siple has a control API and an MCP server, and whatever the window can do, a script or an agent can do. See
[AI agents](/agents/).

### What does Siple cost, and what is the licence?

Siple is free to use at work, to self-host and to modify for your own needs. It is source-available under the Functional
Source License (FSL-1.1-ALv2), not open source in the OSI sense. You cannot sell it or offer it as a competing product or
service. Each version also becomes Apache-2.0 two years after it is released. The summary is on the
[Licence](/project/licence) page; the licence text is what applies.

## Getting help

### Something is not working. What do I try first?

Start with [Troubleshooting](/guide/troubleshooting). It covers the SIP trace and where Siple keeps its logs.

### Where do I report a bug?

Open an issue on [GitHub](https://github.com/zyno-io/siple/issues). Attach a redacted SIP trace if you can. Remove passwords and
tokens first.

### How do I report a security problem?

Report it privately, not as a public issue. The steps are on the [Security](/project/security) page.
