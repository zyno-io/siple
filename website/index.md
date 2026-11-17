---
layout: home
title: Siple
titleTemplate: The scriptable SIP softphone
hero:
  name: Siple
  text: The scriptable SIP softphone
  tagline: A native desktop softphone for macOS, Windows and Linux, built so that people and AI agents can drive the same calls.
  image:
    src: /logo.png
    alt: Siple
  actions:
    - theme: brand
      text: Get started
      link: /guide/install
    - theme: alt
      text: AI agents
      link: /agents/
    - theme: alt
      text: Download
      link: https://github.com/zyno-io/siple/releases
features:
  - title: Multi-line calling
    details: Several calls at once, call waiting, hold and swap, and keyboard shortcuts for all of it.
    link: /guide/calls
  - title: BLF and transfers
    details: Busy-lamp keys with click to call, transfer or directed pickup. Blind and attended transfer with a consult leg.
    link: /guide/extensions
  - title: Agent-first control
    details: A JSON-RPC control API and an MCP server cover every state and action. Agents can also speak into calls and listen.
    link: /agents/
  - title: One config file
    details: Everything lives in config.toml and hot-reloads. The GUI and the API edit the same file and keep your comments.
    link: /guide/configuration
  - title: Resilient networking
    details: Re-registers and moves live calls when your network changes, and after sleep and wake.
    link: /guide/network
  - title: Native and private
    details: Rust and egui, no Electron, no OpenSSL, no telemetry (one optional, anonymous version check). Passwords can live in the system keychain.
    link: /guide/first-run
---

<div class="vp-doc" style="max-width: 1152px; margin: 2rem auto; padding: 0 24px;">

<Shot name="two-lines" alt="Siple with two calls: one active, one on hold" />

## Download

Pre-built packages are published on [GitHub Releases](https://github.com/zyno-io/siple/releases) once release signing is set up. Until a package is listed there, [build from source](/guide/install#build-from-source).

## Licence, in plain words

Siple is **source-available**, not open source. It is released under the [Functional Source License, Version 1.1, ALv2 Future License](/project/licence) (FSL-1.1-ALv2).
You can use, modify, self-host and redistribute it for any purpose except a competing commercial one, and each release becomes
Apache-2.0 two years after it is published. Details and FAQ: [Licence](/project/licence).

</div>
